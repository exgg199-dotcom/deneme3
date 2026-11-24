import CoreFoundation
import Security
import SwiftUI
import UIKit

@MainActor
final class MicrosoftAccount: ObservableObject {
    struct Profile: Codable, Equatable, Identifiable {
        let name: String
        let uuid: String
        var id: String { uuid }

        static func validated(name: String, uuid: String) -> Self? {
            let normalized = uuid.replacingOccurrences(of: "-", with: "").lowercased()
            guard normalized.utf8.count == 32,
                  normalized.utf8.allSatisfy({ (48...57).contains($0) || (97...102).contains($0) }),
                  PlayerName.isValid(name) else {
                return nil
            }
            return Self(name: name, uuid: normalized)
        }
    }
    struct Catalog: Codable { var profiles: [Profile]; var selected: String? }
    enum State: Equatable {
        case idle, code(String, URL), working(String), failed(String)
    }
    @Published private(set) var profile: Profile?
    @Published private(set) var accounts: [Profile] = []
    @Published private(set) var state = State.idle
    private let defaults: UserDefaults
    private let vault: AccountVault
    private let net: AccountNet
    private let now: () -> Date
    private let deviceClock: () -> ContinuousClock.Instant
    private let sleep: (UInt64) async throws -> Void
    private let client: String?
    private let bridge: (String) async -> [String: Any]
    private var signing: Task<Void, Never>?
    private var generation = UUID()
    private var accountEpoch = UUID()
    @MainActor
    private final class Renewal {
        typealias Credentials = (Profile, AccountSecrets)
        var worker: Task<Void, Never>?
        private var result: Result<Credentials, Error>?
        private var waiters: [UUID: CheckedContinuation<Credentials, Error>] = [:]

        func value() async throws -> Credentials {
            let id = UUID()
            return try await withTaskCancellationHandler {
                try Task.checkCancellation()
                if let result { return try result.get() }
                return try await withCheckedThrowingContinuation { waiters[id] = $0 }
            } onCancel: {
                Task { @MainActor in
                    self.waiters.removeValue(forKey: id)?.resume(throwing: CancellationError())
                }
            }
        }

        func finish(_ outcome: Result<Credentials, Error>) {
            guard result == nil else { return }
            result = outcome
            let pending = waiters
            waiters.removeAll()
            for waiter in pending.values { waiter.resume(with: outcome) }
        }

        func cancel() {
            worker?.cancel()
            finish(.failure(CancellationError()))
        }
    }

    private var renewals: [String: Renewal] = [:]
    private var transfer: Task<Void, Error>?
    private var legacyProfile: Profile?
    private enum UnresolvedSelection {
        case missingProfile(String)
        case unreadableCatalog
    }
    private var unresolvedSelection: UnresolvedSelection?
    var needsAccountSelection: Bool { unresolvedSelection != nil }
    nonisolated static var clientID: String? {
        (Bundle.main.object(forInfoDictionaryKey: "HorizonMSAClientID") as? String)
            .map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }
            .flatMap { $0.isEmpty || $0.hasPrefix("$(") ? nil : $0 }
            ?? "331e08f8-172d-43b9-835a-3108c2fcdbf3"
    }
    init(defaults: UserDefaults = .standard, vault: AccountVault = KeychainAccountVault(),
         session: URLSession = .shared, clock: @escaping () -> Date = Date.init,
         deviceClock: @escaping () -> ContinuousClock.Instant = { ContinuousClock.now },
         sleep: @escaping (UInt64) async throws -> Void = { try await Task.sleep(nanoseconds: $0) },
         client: String? = MicrosoftAccount.clientID,
         bridge: @escaping (String) async -> [String: Any] = { await Bridge.send($0) }) {
        self.defaults = defaults; self.vault = vault; net = AccountNet(session: session)
        now = clock; self.deviceClock = deviceClock; self.sleep = sleep; self.client = client; self.bridge = bridge
        let storedCatalog = defaults.object(forKey: "microsoftAccounts.v2")
        let storedLegacy = defaults.object(forKey: "microsoftProfile")
        let legacy = (storedLegacy as? Data).flatMap { try? JSONDecoder().decode(Profile.self, from: $0) }
        if let data = storedCatalog as? Data,
           let saved = try? JSONDecoder().decode(Catalog.self, from: data) {
            accounts = saved.profiles
            profile = accounts.first { $0.uuid == saved.selected }
            if let selected = saved.selected, profile == nil {
                unresolvedSelection = .missingProfile(selected)
            }
        } else if let old = legacy {
            accounts = [old]; profile = old
        } else if storedCatalog != nil || storedLegacy != nil {
            unresolvedSelection = .unreadableCatalog
            state = .failed("Saved accounts could not be read. Sign in again or choose Offline.")
        }
        if let old = legacy, accounts.contains(where: { $0.uuid == old.uuid }) {
            legacyProfile = old
            do { try migrate() } catch { state = .failed(error.localizedDescription) }
        }
    }
    private func saveCatalog() throws {
        let selected: String?
        switch unresolvedSelection {
        case let .missingProfile(id): selected = id
        case .unreadableCatalog: return
        case nil: selected = profile?.uuid
        }
        defaults.set(try JSONEncoder().encode(Catalog(profiles: accounts, selected: selected)),
                     forKey: "microsoftAccounts.v2")
    }
    private func migrate() throws {
        guard let old = legacyProfile, profile?.uuid == old.uuid else { return }
        // Copy successfully before removing either legacy record. A locked
        // Keychain leaves migration retryable and the original credentials intact.
        if try vault.read(old.uuid) == nil {
            guard let secrets = try vault.read("account") else {
                throw AccountError("Sign in again to restore this account.", reauthenticate: true)
            }
            try vault.write(secrets, for: old.uuid)
        }
        try saveCatalog()
        do {
            try vault.remove("account")
            defaults.removeObject(forKey: "microsoftProfile")
            legacyProfile = nil
        } catch {
            // The usable credential is already saved. Keep the marker so a
            // later Play or explicit removal can finish cleaning up.
            Diagnostics.record("Legacy Microsoft account cleanup will retry.")
        }
    }
    func cancel() {
        generation = UUID(); signing?.cancel(); signing = nil; state = .idle
    }
    func select(_ selected: Profile?) {
        let saved = selected.flatMap { requested in accounts.first { $0.uuid == requested.uuid } }
        guard selected == nil || saved != nil else {
            state = .failed("That account is no longer saved. Select another account or choose Offline.")
            return
        }
        cancel()
        accountEpoch = UUID()
        profile = saved
        unresolvedSelection = nil
        do { try saveCatalog() } catch { state = .failed(error.localizedDescription) }
    }
    func remove(_ account: Profile) async {
        cancel()
        do {
            let hasLegacyRecord = legacyProfile?.uuid == account.uuid
            if hasLegacyRecord {
                // A failed legacy deletion must leave the current credential
                // in place. Otherwise migration could copy an older token back.
                try vault.remove("account")
            }
            try vault.remove(account.uuid)
            // A failed removal leaves the account in use. Let any pending
            // refresh persist a Microsoft token that may already have rotated.
            renewals.removeValue(forKey: account.uuid)?.cancel()
            if hasLegacyRecord {
                defaults.removeObject(forKey: "microsoftProfile")
                legacyProfile = nil
            }
            accounts.removeAll { $0.uuid == account.uuid }
            if profile?.uuid == account.uuid { accountEpoch = UUID(); profile = nil }
            try saveCatalog()
        } catch {
            state = .failed(error.localizedDescription)
            return
        }
        if profile == nil, !needsAccountSelection { try? await handOver() }
    }

    func signOut() async {
        select(nil)
        try? await handOver()
    }

    private func checkHandoff(_ epoch: UUID) throws {
        try Task.checkCancellation()
        guard accountEpoch == epoch, signing == nil else { throw CancellationError() }
    }

    /// Every launch awaits the same account refresh. Cached identity alone is
    /// never proof that a usable online session exists.
    func handOver() async throws {
        guard signing == nil else { throw AccountError("Finish or cancel the Microsoft sign-in before playing.") }
        let epoch = accountEpoch
        do {
            guard !needsAccountSelection else { throw AccountError("Select an account or choose Offline before playing.") }
            try migrate()
            let chosen = profile
            let credentials: (Profile, AccountSecrets)?
            if let chosen { credentials = try await self.credentials(for: chosen) }
            else { credentials = nil }
            try checkHandoff(epoch)
            let previous = transfer
            let task = Task { @MainActor in
                _ = try? await previous?.value
                try self.checkHandoff(epoch)
                let command: String
                if let (account, secrets) = credentials {
                    guard secrets.expires > self.now(), !secrets.token.isEmpty else {
                        throw AccountError("Your session expired. Try Play again.")
                    }
                    let data = try JSONSerialization.data(withJSONObject: [
                        "name": account.name, "uuid": account.uuid, "access_token": secrets.token, "user_type": "msa",
                    ])
                    command = "launcher-session " + String(decoding: data, as: UTF8.self)
                } else { command = "launcher-session off" }
                let answer = await self.bridge(command)
                try self.checkHandoff(epoch)
                guard answer.acknowledged, answer.error == nil else {
                    throw AccountError(answer.error ?? "The game could not accept your account. Try Play again.")
                }
            }
            transfer = task
            defer { if transfer == task { transfer = nil } }
            try await withTaskCancellationHandler {
                try await task.value
            } onCancel: {
                task.cancel()
            }
            try checkHandoff(epoch)
            state = .idle
        } catch {
            try checkHandoff(epoch)
            if !(error is CancellationError) { state = .failed(error.localizedDescription) }
            throw error
        }
    }
    private func credentials(for account: Profile) async throws -> (Profile, AccountSecrets) {
        guard Profile.validated(name: account.name, uuid: account.uuid) != nil else {
            throw AccountError("The saved Minecraft profile is invalid. Sign in again to restore this account.",
                               reauthenticate: true)
        }
        if let renewal = renewals[account.uuid] { return try await renewal.value() }
        guard let secrets = try vault.read(account.uuid) else {
            throw AccountError("Sign in again to restore \(account.name).", reauthenticate: true)
        }
        let remaining = secrets.expires.timeIntervalSince(now())
        let canPlay = !secrets.token.isEmpty && remaining > 0
        if canPlay && remaining > 300 { return (account, secrets) }
        guard !secrets.refresh.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
            if canPlay { return (account, secrets) }
            throw AccountError("Sign in again to restore \(account.name).", reauthenticate: true)
        }
        guard let client else {
            if canPlay { return (account, secrets) }
            throw AccountError("Microsoft sign-in isn't configured in this build.")
        }
        let renewal = Renewal()
        renewals[account.uuid] = renewal
        renewal.worker = Task { @MainActor in
            defer {
                if self.renewals[account.uuid] === renewal { self.renewals[account.uuid] = nil }
                renewal.worker = nil
            }
            do {
                renewal.finish(.success(try await self.renew(account, secrets: secrets, client: client)))
            } catch {
                renewal.finish(.failure(error))
            }
        }
        return try await renewal.value()
    }

    private func renew(
        _ account: Profile, secrets: AccountSecrets, client: String
    ) async throws -> (Profile, AccountSecrets) {
        var cached = secrets
        do {
            let microsoft = try await self.refresh(client, secrets.refresh)
            try Task.checkCancellation()
            // Microsoft rotates this credential before Xbox/Minecraft requests.
            // Persist now so a downstream outage cannot discard the new token.
            cached = AccountSecrets(refresh: microsoft.refresh, token: secrets.token, expires: secrets.expires)
            try self.vault.write(cached, for: account.uuid)
            let result = try await self.exchange(microsoft: microsoft)
            try Task.checkCancellation()
            guard result.0.uuid == account.uuid else { throw AccountError("Microsoft returned a different account. Sign in again.", reauthenticate: true) }
            try self.vault.write(result.1, for: account.uuid)
            if let index = self.accounts.firstIndex(where: { $0.uuid == account.uuid }) {
                self.accounts[index] = result.0
                if self.profile?.uuid == account.uuid { self.profile = result.0 }
                try self.saveCatalog()
            }
            return result
        } catch let error as AccountError where error.temporarilyUnavailable {
            // A proactive refresh is optional until the previous Minecraft
            // session expires. Explicit revocation and other errors still fail.
            try Task.checkCancellation()
            guard !cached.token.isEmpty, cached.expires > self.now() else { throw error }
            Diagnostics.record("Microsoft renewal temporarily unavailable; using the still-valid session.")
            return (account, cached)
        }
    }
    func signIn() {
        guard let client else { state = .failed("Microsoft sign-in isn't configured in this build."); return }
        cancel()
        // Supersede any handoff started before this sign-in, even if the
        // sign-in is later cancelled without changing the selected profile.
        accountEpoch = UUID()
        let attempt = generation
        let epoch = accountEpoch
        signing = Task {
            defer { if generation == attempt { signing = nil } }
            do {
                try Task.checkCancellation()
                let tokens = try await deviceCode(client)
                try Task.checkCancellation()
                guard generation == attempt, accountEpoch == epoch else { throw CancellationError() }
                state = .working("Signing in to Minecraft…")
                let (added, secrets) = try await exchange(microsoft: tokens)
                try Task.checkCancellation()
                guard generation == attempt, accountEpoch == epoch else { throw CancellationError() }
                try vault.write(secrets, for: added.uuid)
                // The new credential is durable. A refresh for the old one
                // must neither hold up sign-in nor write after it resumes.
                renewals.removeValue(forKey: added.uuid)?.cancel()
                if let i = accounts.firstIndex(where: { $0.uuid == added.uuid }) { accounts[i] = added }
                else { accounts.append(added) }
                try saveCatalog()
                // Adding does not replace the current selection until all service
                // steps succeeded. Play performs the final, awaited bridge transfer.
                profile = added; unresolvedSelection = nil; accountEpoch = UUID(); try saveCatalog()
                state = .idle; signing = nil; Haptics.done()
            } catch is CancellationError {
                if generation == attempt { state = .idle }
            } catch {
                guard generation == attempt else { return }
                state = .failed(error.localizedDescription); Haptics.failed()
            }
        }
    }
    /// Steps 1 and 2: a code for the player to enter, then the wait for them to do it.
    private func deviceCode(_ client: String) async throws -> (access: String, refresh: String) {
        try Task.checkCancellation()
        state = .working("Asking Microsoft for a code…")
        let start = try await net.form("https://login.microsoftonline.com/consumers/oauth2/v2.0/devicecode",
                                       ["client_id": client, "scope": "XboxLive.signin offline_access"])
        try Task.checkCancellation()
        guard let device = start["device_code"] as? String, !device.isEmpty,
              let code = start["user_code"] as? String, !code.isEmpty,
              let link = (start["verification_uri"] as? String).flatMap(URL.init(string:)),
              link.scheme == "https", link.host?.isEmpty == false
        else { throw AccountError("Microsoft didn't give a sign-in code.") }
        func seconds(_ key: String, _ fallback: Int) -> Int {
            let value = start[key] ?? fallback
            guard let number = value as? NSNumber,
                  CFGetTypeID(number) != CFBooleanGetTypeID() else { return 0 }
            return number as? Int ?? 0
        }
        let delay = seconds("interval", 5)
        let lifetime = seconds("expires_in", 900)
        let maximumInterval = UInt64.max / 1_000_000_000
        guard delay > 0, UInt64(delay) <= maximumInterval, lifetime > 0 else {
            throw AccountError("Microsoft returned invalid sign-in code timing.")
        }
        var interval = UInt64(delay)
        let deadline = deviceClock().advanced(by: .seconds(lifetime))
        state = .code(code, link)
        var retryDelay: TimeInterval = 0
        while true {
            let remaining = deviceClock().duration(to: deadline)
            guard remaining > .zero else { break }
            let parts = remaining.components
            let seconds = Double(parts.seconds) + Double(parts.attoseconds) / 1e18
            let wait = min(max(Double(interval), retryDelay), seconds, Double(maximumInterval))
            try await sleep(UInt64((wait * 1_000_000_000).rounded(.up)))
            retryDelay = 0
            try Task.checkCancellation()
            guard deviceClock() < deadline else { break }
            let answer: [String: Any]
            do {
                answer = try await net.form(
                    "https://login.microsoftonline.com/consumers/oauth2/v2.0/token",
                    ["grant_type": "urn:ietf:params:oauth:grant-type:device_code", "client_id": client,
                     "device_code": device],
                    allowErrors: true,
                    deadline: deadline
                )
            } catch let error as AccountError where error.temporarilyUnavailable {
                // Temporary transport failures do not settle the device grant.
                // Back off before the next poll and keep the same expiry.
                interval = min(maximumInterval, interval * 2)
                continue
            }
            guard deviceClock() < deadline else { break }
            retryDelay = AccountNet.retryDelay(answer[AccountNet.retryAfterHeader] as? String, now: now())
            switch answer["error"] as? String {
            case "slow_down":
                guard interval <= maximumInterval - 5 else {throw AccountError("Microsoft returned an invalid sign-in polling interval.")}
                interval += 5
            case _ where AccountError.temporaryHTTP(answer["status"] as? Int):
                interval = min(maximumInterval, interval * 2)
                continue
            case "authorization_pending": continue
            case "authorization_declined", "access_denied": throw AccountError("The sign-in was declined.")
            case "expired_token": throw AccountError("The code expired. Try again.")
            case nil:
                if let status = answer["status"] as? Int {
                    throw AccountError("Microsoft's sign-in service is unavailable (\(status)). Try again.")
                }
                guard let access = Self.credential(answer["access_token"]),
                      let refresh = Self.credential(answer["refresh_token"])
                else { throw AccountError("Microsoft's answer had no token.") }
                return (access, refresh)
            default: throw AccountError("Microsoft refused the sign-in. Try again or check this build’s app configuration.")
            }
        }
        throw AccountError("The code expired. Try again.")
    }

    private func refresh(_ client: String, _ token: String) async throws -> (access: String, refresh: String) {
        let answer = try await net.form(
            "https://login.microsoftonline.com/consumers/oauth2/v2.0/token",
            ["grant_type": "refresh_token", "client_id": client, "refresh_token": token,
             "scope": "XboxLive.signin offline_access"],
            allowErrors: true
        )
        let temporary = AccountError.temporaryHTTP(answer["status"] as? Int)
        if let error = answer["error"] as? String {
            if !temporary && ["invalid_grant", "interaction_required", "consent_required"].contains(error) {
                throw AccountError("Microsoft requires you to sign in again. Your saved account has been kept.", reauthenticate: true)
            }
            throw AccountError("Microsoft could not renew the session. Check your connection and try again.",
                               temporarilyUnavailable: temporary)
        }
        guard answer["status"] == nil,
              let access = Self.credential(answer["access_token"]) else {
            throw AccountError("Microsoft returned an incomplete session. Try again.",
                               temporarilyUnavailable: temporary)
        }
        // A refresh response may leave the existing credential in place.
        // Treat an empty field like an omitted one so it cannot erase the saved token.
        let replacement = Self.credential(answer["refresh_token"])
        return (access, replacement ?? token)
    }

    private static func credential(_ value: Any?) -> String? {
        guard let token = value as? String, !token.isEmpty,
              !token.contains(where: \.isWhitespace) else { return nil }
        return token
    }

    /// Steps 3 to 7: Xbox Live, XSTS, Minecraft, ownership and the profile; then keep it all.
    private func exchange(microsoft: (access: String, refresh: String)) async throws -> (Profile, AccountSecrets) {
        let xbl = try await net.json("https://user.auth.xboxlive.com/user/authenticate", [
            "Properties": ["AuthMethod": "RPS", "SiteName": "user.auth.xboxlive.com",
                           "RpsTicket": "d=\(microsoft.access)"],
            "RelyingParty": "http://auth.xboxlive.com", "TokenType": "JWT",
        ])
        let claims = (xbl["DisplayClaims"] as? [String: Any])?["xui"] as? [[String: Any]]
        guard let xblToken = Self.credential(xbl["Token"]),
              let uhs = Self.credential(claims?.first?["uhs"])
        else { throw AccountError("Xbox Live didn't sign you in.") }
        let xsts = try await net.json("https://xsts.auth.xboxlive.com/xsts/authorize", [
            "Properties": ["SandboxId": "RETAIL", "UserTokens": [xblToken]] as [String: Any],
            "RelyingParty": "rp://api.minecraftservices.com/", "TokenType": "JWT",
        ], allowErrors: true)
        let xstsTemporary = AccountError.temporaryHTTP(xsts["status"] as? Int)
        if !xstsTemporary, let code = xsts["XErr"] as? Int {
            throw AccountError(Self.xboxError(code))
        }
        guard xsts["status"] == nil, let xstsToken = Self.credential(xsts["Token"]) else {
            throw AccountError("Xbox Live refused the sign-in.",
                               temporarilyUnavailable: xstsTemporary)
        }
        let minecraft = try await net.json("https://api.minecraftservices.com/authentication/login_with_xbox",
                                           ["identityToken": "XBL3.0 x=\(uhs);\(xstsToken)"], allowErrors: true)
        guard minecraft["status"] == nil, let token = Self.credential(minecraft["access_token"]) else {
            let status = minecraft["status"] as? Int
            let temporary = AccountError.temporaryHTTP(status)
            if !temporary && (status == 403 || (minecraft["errorMessage"] as? String)?.contains("Invalid app") == true) {
                throw AccountError("This app is not approved by Mojang yet.")
            }
            throw AccountError("Minecraft refused the sign-in.",
                               temporarilyUnavailable: temporary)
        }
        guard let lifetime = minecraft["expires_in"] as? NSNumber,
              CFGetTypeID(lifetime) != CFBooleanGetTypeID(),
              lifetime.doubleValue.isFinite, lifetime.doubleValue > 0 else {
            throw AccountError("Minecraft did not provide a valid session expiry. Try again.")
        }
        let expires = now().addingTimeInterval(lifetime.doubleValue)
        let store = try await net.get("https://api.minecraftservices.com/entitlements/mcstore", token: token)
        guard let entitlements = store["items"] as? [[String: Any]] else {
            throw AccountError("Minecraft could not confirm game ownership. Try again.")
        }
        let owned = entitlements.contains {
            ["game_minecraft", "product_minecraft"].contains($0["name"] as? String ?? "")
        }
        guard owned else { throw AccountError("This account doesn't own Minecraft: Java Edition.") }
        let profile = try await net.get("https://api.minecraftservices.com/minecraft/profile", token: token,
                                        allowErrors: true)
        if let status = profile["status"] as? Int {
            if status == 404 {
                throw AccountError("Create your Minecraft profile on minecraft.net first.")
            }
            throw AccountError("Minecraft could not load your profile (\(status)). Try again.",
                               temporarilyUnavailable: AccountError.temporaryHTTP(status))
        }
        guard let id = profile["id"] as? String, let name = profile["name"] as? String else {
            throw AccountError("Minecraft returned an incomplete account profile. Try again.")
        }
        try Task.checkCancellation()
        guard let validated = Profile.validated(name: name, uuid: id) else {
            throw AccountError("Minecraft returned an invalid account profile.")
        }
        guard expires > now() else {
            throw AccountError("The Minecraft session expired during sign-in. Try again.")
        }
        return (validated, AccountSecrets(refresh: microsoft.refresh, token: token, expires: expires))
    }

    private static func xboxError(_ code: Int) -> String {
        switch code {
        case 2_148_916_233: return "This Microsoft account has no Xbox profile yet: create one at xbox.com."
        case 2_148_916_235: return "Xbox Live isn't available in your country."
        case 2_148_916_236, 2_148_916_237: return "This account needs adult verification on xbox.com."
        case 2_148_916_238: return "This is a child account: an adult has to add it to a Microsoft family."
        default: return "Xbox Live refused the sign-in (\(code))."
        }
    }
}

// MARK: - The card

/// The Microsoft account on the Play page: who is signed in, or the way to sign in.
struct AccountCard: View {
    @ObservedObject var account: MicrosoftAccount

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(spacing: 10) {
                Image(systemName: "person.badge.key.fill").foregroundStyle(Theme.accentText)
                VStack(alignment: .leading, spacing: 2) {
                    Text("MICROSOFT ACCOUNT").font(Theme.mono(8)).foregroundStyle(Theme.textMuted)
                    Text(account.profile?.name ?? "Not signed in").font(Theme.sans(15, .semiBold))
                        .foregroundStyle(Theme.text)
                }
                Spacer()
                if account.profile != nil {
                    Button("Use offline") { Task { await account.signOut() } }
                        .font(Theme.sans(12, .semiBold)).foregroundStyle(Theme.textMuted)
                        .frame(minHeight: 44)
                }
            }
            if account.needsAccountSelection {
                SecondaryButton(title: "Use offline profile") { Task { await account.signOut() } }
            }
            if !account.accounts.isEmpty {
                ForEach(account.accounts) { saved in
                    HStack {
                        Button { account.select(saved) } label: {
                            HStack {
                                Image(systemName: account.profile?.uuid == saved.uuid ? "checkmark.circle.fill" : "circle")
                                Text(saved.name).font(.body.weight(.medium))
                                Spacer()
                            }.frame(minHeight: 44)
                        }.buttonStyle(.plain)
                            .accessibilityIdentifier("account-select-" + saved.uuid)
                        Button(role: .destructive) { Task { await account.remove(saved) } } label: {
                            Image(systemName: "trash").frame(width: 44, height: 44)
                        }.buttonStyle(.plain).accessibilityLabel("Remove " + saved.name)
                    }
                }
            }
            switch account.state {
            case let .code(code, link):
                VStack(alignment: .leading, spacing: 8) {
                    Text("Enter this code at \(link.host ?? "microsoft.com/link"):")
                        .font(Theme.sans(12)).foregroundStyle(Theme.textMuted)
                    Text(code).font(Theme.pixel(30)).foregroundStyle(Theme.text).textSelection(.enabled)
                    PrimaryButton(title: "Copy code and open", icon: "safari") {
                        UIPasteboard.general.string = code
                        UIApplication.shared.open(link)
                    }
                    SecondaryButton(title: "Cancel") { account.cancel() }
                }
            case let .working(step):
                HStack(spacing: 8) {
                    ProgressView().tint(Theme.accentText)
                    Text(step).font(Theme.sans(12)).foregroundStyle(Theme.textMuted)
                }
                SecondaryButton(title: "Cancel") { account.cancel() }
            case let .failed(message):
                Label(message, systemImage: "exclamationmark.triangle.fill")
                    .font(Theme.sans(12)).foregroundStyle(Theme.warn)
                    .fixedSize(horizontal: false, vertical: true)
            case .idle:
                EmptyView()
            }
            if !isSigning {
                if MicrosoftAccount.clientID != nil {
                    SecondaryButton(title: account.accounts.isEmpty ? "Sign in with Microsoft" : "Add Microsoft account", icon: "key.fill") { account.signIn() }
                    Text("For servers that check accounts (online mode). Single player and LAN don't need it.")
                        .font(Theme.sans(11)).foregroundStyle(Theme.textFaint)
                } else {
                    Text("Microsoft sign-in is unavailable in this build. The app developer must configure an approved application ID. Single player and offline servers are available.")
                        .font(Theme.sans(11)).foregroundStyle(Theme.textFaint)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
        }
        .padding(14)
        .card()
    }

    private var isSigning: Bool {
        switch account.state {
        case .code, .working: return true
        default: return false
        }
    }
}
