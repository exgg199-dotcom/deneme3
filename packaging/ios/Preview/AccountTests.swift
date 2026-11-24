import XCTest
import Combine
@testable import LauncherPreview

/// Only a presence marker for the in-memory engine; never actual Minecraft assets.
private final class PreparedGameFilesFixture {
    private var created: URL?
    init() throws {
        let marker = GameFolder.resources.appendingPathComponent("horizon-resources.json")
        if !FileManager.default.fileExists(atPath: marker.path) {
            try FileManager.default.createDirectory(at: marker.deletingLastPathComponent(),
                                                    withIntermediateDirectories: true)
            try Data("{}".utf8).write(to: marker, options: .withoutOverwriting)
            created = marker
        }
    }
    func cleanUp() throws {
        // Preserve a marker/resources already supplied by the preview host.
        if let marker = created {
            try FileManager.default.removeItem(at: marker)
            created = nil
        }
    }
}

private final class MemoryAccountVault: AccountVault {
    var values: [String: AccountSecrets] = [:]
    var failWrites = false
    var refusedRemovals: Set<String> = []
    func read(_ account: String) throws -> AccountSecrets? { values[account] }
    func write(_ secrets: AccountSecrets, for account: String) throws {
        if failWrites { throw AccountError("Fixture storage failure") }
        values[account] = secrets
    }
    func remove(_ account: String) throws {
        if refusedRemovals.contains(account) { throw AccountError("Fixture removal failure") }
        values.removeValue(forKey: account)
    }
}

private final class AuthURLProtocol: URLProtocol {
    static var failure: Error?
    static var rawBody: ((URLRequest) -> Data?)?
    static var retryAfter: ((Int) -> String?)?
    static var stall = false
    static var stallRequest: ((URLRequest) -> Bool)?
    static var response: (URLRequest) -> (Int, [String: Any]) = { _ in (500, [:]) }
    override class func canInit(with request: URLRequest) -> Bool { true }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
    override func startLoading() {
        if Self.stall || Self.stallRequest?(request) == true { return }
        if let error = Self.failure { client?.urlProtocol(self, didFailWithError: error); return }
        let (status, body) = Self.response(request)
        var headers = ["Content-Type": "application/json"]
        headers["Retry-After"] = Self.retryAfter?(status)
        let response = HTTPURLResponse(url: request.url!, statusCode: status, httpVersion: nil, headerFields: headers)!
        client?.urlProtocol(self, didReceive: response, cacheStoragePolicy: .notAllowed)
        let data = Self.rawBody?(request) ?? (try! JSONSerialization.data(withJSONObject: body))
        client?.urlProtocol(self, didLoad: data)
        client?.urlProtocolDidFinishLoading(self)
    }
    override func stopLoading() {}
}

@MainActor
final class AccountTests: XCTestCase {
    private let alice = MicrosoftAccount.Profile(name: "Alice", uuid: String(repeating: "1", count: 32))
    private let bob = MicrosoftAccount.Profile(name: "Bob", uuid: String(repeating: "2", count: 32))
    private let instant = Date(timeIntervalSince1970: 2_000_000_000)
    private var suite: String!
    private var defaults: UserDefaults!
    private var vault: MemoryAccountVault!
    private var session: URLSession!
    private var requests: [String] = []
    private var bridgeCommands: [String] = []
    private let lock = NSLock()
    private var preparedGameFiles: PreparedGameFilesFixture?

    override func setUp() async throws {
        suite = "horizon.auth.fixture." + UUID().uuidString
        defaults = UserDefaults(suiteName: suite)!
        vault = MemoryAccountVault()
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [AuthURLProtocol.self]
        session = URLSession(configuration: configuration)
        requests = []; bridgeCommands = []; AuthURLProtocol.failure = nil
    }
    override func tearDown() async throws {
        try preparedGameFiles?.cleanUp()
        preparedGameFiles = nil
        session.invalidateAndCancel()
        defaults.removePersistentDomain(forName: suite)
        AuthURLProtocol.response = { _ in (500, [:]) }; AuthURLProtocol.failure = nil; AuthURLProtocol.rawBody = nil
        AuthURLProtocol.retryAfter = nil
        AuthURLProtocol.stall = false; AuthURLProtocol.stallRequest = nil
    }
    private func seed(expired: Bool = true) throws {
        defaults.set(try JSONEncoder().encode(MicrosoftAccount.Catalog(profiles: [alice, bob], selected: alice.uuid)), forKey: "microsoftAccounts.v2")
        vault.values[alice.uuid] = AccountSecrets(refresh: "fixture-old-refresh", token: "fixture-old-access", expires: instant.addingTimeInterval(expired ? -86400 : 7200))
        vault.values[bob.uuid] = AccountSecrets(refresh: "fixture-bob-refresh", token: "fixture-bob-access", expires: instant.addingTimeInterval(7200))
    }
    private func model(bridgeOK: Bool = true) -> MicrosoftAccount {
        MicrosoftAccount(defaults: defaults, vault: vault, session: session, clock: { self.instant }, sleep: { _ in await Task.yield() }, client: "fixture-client") { command in
            self.bridgeCommands.append(command)
            return bridgeOK ? ["ok": true] : ["error": "fixture failure"]
        }
    }
    private func network(tokenError: Bool = false, downstreamError: Bool = false, expiry: Bool = true) {
        let uuid = alice.uuid
        AuthURLProtocol.response = { request in
            let path = request.url!.path
            self.lock.lock(); self.requests.append(path); self.lock.unlock()
            if path.hasSuffix("/token") {
                return tokenError ? (400, ["error": "invalid_grant"]) : (200, ["access_token": "fixture-ms-access", "refresh_token": "fixture-rotated"])
            }
            if path == "/user/authenticate" { return (200, ["Token": "fixture-xbl", "DisplayClaims": ["xui": [["uhs": "fixture"]]]]) }
            if path == "/xsts/authorize" { return downstreamError ? (503, ["error": "unavailable"]) : (200, ["Token": "fixture-xsts"]) }
            if path == "/authentication/login_with_xbox" {
                var answer: [String: Any] = ["access_token": "fixture-new-access"]
                if expiry { answer["expires_in"] = 3600 }
                return (200, answer)
            }
            if path == "/entitlements/mcstore" { return (200, ["items": [["name": "game_minecraft"]]]) }
            return (200, ["id": uuid, "name": "AliceRenewed"])
        }
    }
    func testExpiredRelaunchAndConcurrentPlayShareRefresh() async throws {
        try seed(); network(); let account = model()
        async let first: Void = account.handOver()
        async let second: Void = account.handOver()
        try await first; try await second
        XCTAssertEqual(requests.filter { $0.hasSuffix("/token") }.count, 1)
        XCTAssertEqual(vault.values[alice.uuid]?.refresh, "fixture-rotated")
        XCTAssertEqual(account.profile?.name, "AliceRenewed")
        XCTAssertTrue(bridgeCommands.allSatisfy { $0.contains("fixture-new-access") })
        let metadata = String(decoding: defaults.data(forKey: "microsoftAccounts.v2")!, as: UTF8.self)
        XCTAssertFalse(metadata.contains("fixture-"))
    }
    func testRotationPersistsBeforeDownstreamFailureWithoutSigningOut() async throws {
        try seed(); network(downstreamError: true); let account = model()
        do { try await account.handOver(); XCTFail("must block Play") } catch {
            XCTAssertFalse((error as? AccountError)?.reauthenticate ?? true)
        }
        XCTAssertEqual(vault.values[alice.uuid]?.refresh, "fixture-rotated")
        XCTAssertEqual(account.profile, alice); XCTAssertEqual(account.accounts.count, 2)
        XCTAssertTrue(bridgeCommands.isEmpty)
    }
    func testTemporaryRefreshFailureKeepsAStillValidMinecraftSession() async throws {
        try seed()
        vault.values[alice.uuid] = AccountSecrets(refresh: "fixture-old-refresh", token: "fixture-old-access",
                                                 expires: instant.addingTimeInterval(90))
        network(downstreamError: true)
        let account = model()

        try await account.handOver()

        XCTAssertEqual(account.profile, alice)
        XCTAssertEqual(account.state, .idle)
        XCTAssertEqual(vault.values[alice.uuid]?.refresh, "fixture-rotated")
        XCTAssertEqual(vault.values[alice.uuid]?.token, "fixture-old-access")
        XCTAssertEqual(bridgeCommands.count, 1)
        XCTAssertTrue(bridgeCommands[0].contains("fixture-old-access"))
    }

    func testDNSFailureKeepsValidSession() async throws {
        try seed()
        vault.values[alice.uuid] = AccountSecrets(refresh: "refresh", token: "cached",
                                                 expires: instant.addingTimeInterval(90))
        AuthURLProtocol.failure = URLError(.dnsLookupFailed)
        try await model().handOver()
        XCTAssertTrue(bridgeCommands.first?.contains("cached") == true)
    }

    func testTemporaryStatusTakesPrecedenceOverAnErrorBodyDuringRenewal() async throws {
        let outages: [(String, Int, [String: Any], String)] = [
            ("/oauth2/v2.0/token", 503, ["error": "invalid_grant"], "fixture-old-refresh"),
            ("/xsts/authorize", 429, ["XErr": 2_148_916_233], "fixture-rotated"),
            ("/authentication/login_with_xbox", 503, ["errorMessage": "Invalid app"], "fixture-rotated"),
        ]
        for (endpoint, status, body, refreshToken) in outages {
            try seed()
            vault.values[alice.uuid] = AccountSecrets(refresh: "fixture-old-refresh", token: "fixture-old-access",
                                                      expires: instant.addingTimeInterval(90))
            network()
            let response = AuthURLProtocol.response
            AuthURLProtocol.response = { request in
                if request.url?.path.hasSuffix(endpoint) == true { return (status, body) }
                return response(request)
            }
            bridgeCommands.removeAll()
            let account = model()

            try await account.handOver()

            XCTAssertEqual(account.profile, alice)
            XCTAssertEqual(vault.values[alice.uuid]?.refresh, refreshToken)
            XCTAssertEqual(vault.values[alice.uuid]?.token, "fixture-old-access")
            XCTAssertEqual(bridgeCommands.count, 1)
            XCTAssertTrue(bridgeCommands.first?.contains("fixture-old-access") == true)
        }
    }

    func testExplicitRefreshDenialDoesNotReuseTheCachedSession() async throws {
        try seed()
        vault.values[alice.uuid] = AccountSecrets(refresh: "fixture-old-refresh", token: "fixture-old-access",
                                                 expires: instant.addingTimeInterval(90))
        network(tokenError: true)
        let account = model()

        do {
            try await account.handOver()
            XCTFail("Microsoft's explicit denial must stop Play")
        } catch {
            XCTAssertTrue((error as? AccountError)?.reauthenticate == true)
        }
        XCTAssertTrue(bridgeCommands.isEmpty)
    }

    func testBlankReplacementTokenKeepsThePreviousRefreshCredential() async throws {
        try seed()
        network()
        let response = AuthURLProtocol.response
        AuthURLProtocol.response = { request in
            if request.url!.path.hasSuffix("/token") {
                return (200, ["access_token": "fixture-ms-access", "refresh_token": "   "])
            }
            return response(request)
        }
        let account = model()
        try await account.handOver()
        XCTAssertEqual(vault.values[alice.uuid]?.refresh, "fixture-old-refresh")
        XCTAssertEqual(vault.values[alice.uuid]?.token, "fixture-new-access")
    }

    func testSessionNearExpiryIsUsableWhenItsRefreshCredentialIsMissing() async throws {
        try seed()
        vault.values[alice.uuid] = AccountSecrets(refresh: "", token: "fixture-old-access",
                                                expires: instant.addingTimeInterval(90))
        network()
        let account = model()
        try await account.handOver()
        XCTAssertTrue(requests.isEmpty)
        XCTAssertEqual(bridgeCommands.count, 1)
        XCTAssertTrue(bridgeCommands[0].contains("fixture-old-access"))
    }

    func testBooleanSessionLifetimeCannotReplaceACachedToken() async throws {
        try seed()
        network()
        let response = AuthURLProtocol.response
        AuthURLProtocol.response = { request in
            if request.url?.path == "/authentication/login_with_xbox" {
                return (200, ["access_token": "fixture-replacement", "expires_in": true])
            }
            return response(request)
        }
        let account = model()
        do {
            try await account.handOver()
            XCTFail("A Boolean lifetime cannot establish a session")
        } catch {
            XCTAssertTrue(error.localizedDescription.contains("valid session expiry"))
        }
        XCTAssertEqual(vault.values[alice.uuid]?.token, "fixture-old-access")
        XCTAssertTrue(bridgeCommands.isEmpty)
    }

    func testExpiredAccountWithBlankCredentialAsksToSignInWithoutRequestingTokens() async throws {
        try seed()
        vault.values[alice.uuid] = AccountSecrets(refresh: "", token: "fixture-old-access",
                                                expires: instant.addingTimeInterval(-300))
        network()
        let account = model()
        do {
            try await account.handOver()
            XCTFail("An expired account with no refresh token cannot play online")
        } catch {
            XCTAssertTrue((error as? AccountError)?.reauthenticate == true)
        }
        XCTAssertTrue(requests.isEmpty)
        XCTAssertTrue(bridgeCommands.isEmpty)
        XCTAssertEqual(account.profile, alice)
    }

    func testInitialSignInDoesNotReplaceCredentialsWhenRefreshTokenIsBlank() async throws {
        try seed(expired: false)
        let tokenRequested = expectation(description: "device token requested")
        let original = vault.values[alice.uuid]
        AuthURLProtocol.response = { request in
            if request.url!.path.hasSuffix("/devicecode") {
                return (200, ["device_code": "fixture-device", "user_code": "FIXTURE",
                              "verification_uri": "https://microsoft.com/link", "expires_in": 900, "interval": 1])
            }
            if request.url!.path.hasSuffix("/token") {
                tokenRequested.fulfill()
                return (200, ["access_token": "fixture-ms-access", "refresh_token": ""])
            }
            XCTFail("An incomplete token response must stop before the Xbox exchange")
            return (503, [:])
        }
        let account = model()
        account.signIn()
        defer { account.cancel() }
        await fulfillment(of: [tokenRequested], timeout: 2)
        for _ in 0..<100 {
            if case .failed = account.state { break }
            try await Task.sleep(nanoseconds: 10_000_000)
        }
        XCTAssertEqual(account.state, .failed("Microsoft's answer had no token."))
        XCTAssertEqual(account.profile, alice)
        XCTAssertEqual(vault.values[alice.uuid], original)
    }

    func testInvalidGrantPreservesAccountAndAsksForReauthentication() async throws {
        try seed(); network(tokenError: true); let account = model()
        do { try await account.handOver(); XCTFail("must block Play") } catch {
            XCTAssertTrue((error as? AccountError)?.reauthenticate ?? false)
        }
        XCTAssertEqual(account.profile, alice)
        XCTAssertEqual(vault.values[alice.uuid]?.refresh, "fixture-old-refresh")
        XCTAssertTrue(bridgeCommands.isEmpty)
    }
    func testMissingEntitlementsAreDistinguishedFromAnAccountWithoutJavaEdition() async throws {
        let replies: [([String: Any], String)] = [
            ([:], "Minecraft could not confirm game ownership. Try again."),
            (["items": []], "This account doesn't own Minecraft: Java Edition."),
        ]
        for (body, message) in replies {
            try seed()
            network()
            let response = AuthURLProtocol.response
            AuthURLProtocol.response = { request in
                if request.url!.path == "/entitlements/mcstore" { return (200, body) }
                return response(request)
            }
            let account = model()
            do { try await account.handOver(); XCTFail("Play should wait for verified ownership") }
            catch { XCTAssertEqual(error.localizedDescription, message) }
            XCTAssertEqual(vault.values[alice.uuid]?.token, "fixture-old-access")
            XCTAssertEqual(account.profile, alice)
        }
        XCTAssertTrue(bridgeCommands.isEmpty)
    }

    func testRenewalRejectsProfilesTheGameCannotUse() async throws {
        XCTAssertEqual(
            MicrosoftAccount.Profile.validated(name: "Alice", uuid: "069A79F4-44E9-4726-A5BE-FCA90E38AAF5")?.uuid,
            "069a79f444e94726a5befca90e38aaf5"
        )
        for (id, name) in [
            (String(repeating: "ａ", count: 32), "Alice"),
            (alice.uuid, "Name with spaces"),
            (alice.uuid, String(repeating: "a", count: 17)),
        ] {
            try seed()
            network()
            let response = AuthURLProtocol.response
            AuthURLProtocol.response = { request in
                if request.url?.path == "/minecraft/profile" { return (200, ["id": id, "name": name]) }
                return response(request)
            }
            let account = model()
            do {
                try await account.handOver()
                XCTFail("An unusable Minecraft profile must stop Play")
            } catch {
                XCTAssertEqual(error.localizedDescription, "Minecraft returned an invalid account profile.")
            }
            XCTAssertEqual(account.profile, alice)
            XCTAssertEqual(vault.values[alice.uuid]?.token, "fixture-old-access")
            XCTAssertTrue(bridgeCommands.isEmpty)
        }
    }

    func testInvalidSavedProfileAsksForSignInWithoutUsingItsToken() async throws {
        let invalid = MicrosoftAccount.Profile(name: "Name with spaces", uuid: alice.uuid)
        defaults.set(try JSONEncoder().encode(MicrosoftAccount.Catalog(profiles: [invalid], selected: invalid.uuid)),
                     forKey: "microsoftAccounts.v2")
        vault.values[invalid.uuid] = AccountSecrets(refresh: "fixture-refresh", token: "fixture-access",
                                                    expires: instant.addingTimeInterval(3600))
        let account = model()
        do {
            try await account.handOver()
            XCTFail("An invalid saved profile must stop Play")
        } catch {
            XCTAssertTrue((error as? AccountError)?.reauthenticate == true)
        }
        XCTAssertNotNil(vault.values[invalid.uuid])
        XCTAssertTrue(bridgeCommands.isEmpty)
    }

    func testProfileOutageDoesNotAskThePlayerToCreateAProfile() async throws {
        for (status, message) in [
            (503, "Minecraft could not load your profile (503). Try again."),
            (404, "Create your Minecraft profile on minecraft.net first."),
            (200, "Minecraft returned an incomplete account profile. Try again."),
        ] {
            try seed()
            network()
            let response = AuthURLProtocol.response
            AuthURLProtocol.response = { request in
                if request.url!.path == "/minecraft/profile" { return (status, [:]) }
                return response(request)
            }
            AuthURLProtocol.rawBody = { request in
                request.url!.path == "/minecraft/profile" && status != 200
                    ? Data("Non-JSON service error".utf8) : nil
            }
            let account = model()
            do { try await account.handOver(); XCTFail("An unavailable profile must block Play") }
            catch { XCTAssertEqual(error.localizedDescription, message) }
            XCTAssertEqual(vault.values[alice.uuid]?.refresh, "fixture-rotated")
            XCTAssertEqual(vault.values[alice.uuid]?.token, "fixture-old-access")
            XCTAssertEqual(account.profile, alice)
        }
        XCTAssertTrue(bridgeCommands.isEmpty)
    }

    func testUnknownExpiryIsNotAcceptedAsOneDay() async throws {
        try seed(); network(expiry: false); let account = model()
        do { try await account.handOver(); XCTFail("missing expiry must fail") } catch {}
        XCTAssertEqual(vault.values[alice.uuid]?.refresh, "fixture-rotated")
        XCTAssertEqual(vault.values[alice.uuid]?.token, "fixture-old-access")
        XCTAssertTrue(bridgeCommands.isEmpty)
    }
    func testBridgeFailureDoesNotUncoverOrStartGame() async throws {
        try seed(expired: false); let account = model(bridgeOK: false)
        let game = GameLink(); var uncovered = false
        game.prepareSession = { try await account.handOver() }; game.uncover = { uncovered = true }
        await game.play()
        XCTAssertNotNil(game.problem); XCTAssertFalse(uncovered)
        XCTAssertEqual(game.phase, .unknown); XCTAssertNil(game.launching)
    }
    func testLegacyMigrationRetriesWithoutDeletingOnWriteFailure() async throws {
        defaults.set(try JSONEncoder().encode(alice), forKey: "microsoftProfile")
        let secrets = AccountSecrets(refresh: "fixture-old", token: "fixture-valid", expires: instant.addingTimeInterval(7200))
        vault.values["account"] = secrets; vault.failWrites = true
        let account = model()
        XCTAssertEqual(vault.values["account"], secrets)
        XCTAssertNil(defaults.data(forKey: "microsoftAccounts.v2"))
        vault.failWrites = false; try await account.handOver()
        XCTAssertEqual(vault.values[alice.uuid], secrets); XCTAssertNil(vault.values["account"])
        XCTAssertNil(defaults.data(forKey: "microsoftProfile"))
        XCTAssertEqual(model().profile, alice)
    }

    func testUnusableLegacyAccountDoesNotBlockAnotherAccountOrOfflinePlay() async throws {
        try seed(expired: false)
        defaults.set(try JSONEncoder().encode(MicrosoftAccount.Catalog(
            profiles: [alice, bob], selected: bob.uuid)), forKey: "microsoftAccounts.v2")
        defaults.set(try JSONEncoder().encode(alice), forKey: "microsoftProfile")
        vault.values[alice.uuid] = nil

        let account = model()
        XCTAssertEqual(account.state, .idle)
        try await account.handOver()
        XCTAssertTrue(bridgeCommands.last?.contains("fixture-bob-access") == true)
        await account.signOut()
        XCTAssertEqual(bridgeCommands.last, "launcher-session off")
        account.select(alice)
        do {
            try await account.handOver()
            XCTFail("the selected legacy account has no credential")
        } catch {
            XCTAssertTrue((error as? AccountError)?.reauthenticate == true)
        }
        XCTAssertEqual(bridgeCommands.count, 2)
    }

    func testCurrentCredentialIsUsableWhileLegacyCleanupIsDeferred() async throws {
        defaults.set(try JSONEncoder().encode(alice), forKey: "microsoftProfile")
        vault.values["account"] = AccountSecrets(refresh: "fixture-legacy", token: "fixture-usable",
                                                  expires: instant.addingTimeInterval(7200))
        vault.refusedRemovals = ["account"]
        let account = model()
        try await account.handOver()
        XCTAssertTrue(bridgeCommands.last?.contains("fixture-usable") == true)
        XCTAssertNotNil(vault.values[alice.uuid])
        XCTAssertNotNil(vault.values["account"])
        XCTAssertNotNil(defaults.data(forKey: "microsoftProfile"))

        vault.refusedRemovals = []
        try await account.handOver()
        XCTAssertNil(vault.values["account"])
        XCTAssertNil(defaults.data(forKey: "microsoftProfile"))
    }
    func testSwitchDuringRefreshCannotTransferOldSelection() async throws {
        try seed(); network(); let account = model()
        let requested = expectation(description: "refresh started")
        let gate = DispatchSemaphore(value: 0)
        let response = AuthURLProtocol.response
        AuthURLProtocol.response = { request in
            if request.url!.path.hasSuffix("/token") {
                requested.fulfill(); _ = gate.wait(timeout: .now() + 5)
            }
            return response(request)
        }
        let task = Task { try await account.handOver() }
        await fulfillment(of: [requested], timeout: 3)
        account.select(bob)
        gate.signal()
        do { try await task.value } catch is CancellationError {} catch { XCTFail("\(error)") }
        try await account.handOver()
        XCTAssertEqual(account.profile, bob)
        XCTAssertTrue(bridgeCommands.allSatisfy { $0.contains("fixture-bob-access") })
    }
    func testCancellingPlayStopsWaitingWithoutCancellingTheSharedRefresh() async throws {
        try seed()
        network()
        let account = model()
        let requested = expectation(description: "shared refresh reached Microsoft")
        let cancelled = expectation(description: "first Play stopped waiting")
        let release = DispatchSemaphore(value: 0)
        let response = AuthURLProtocol.response
        AuthURLProtocol.response = { request in
            if request.url!.path.hasSuffix("/token") {
                requested.fulfill()
                _ = release.wait(timeout: .now() + 5)
            }
            return response(request)
        }
        let first = Task {
            do {
                try await account.handOver()
                XCTFail("Cancelled Play should not hand over the session")
            } catch is CancellationError {
                cancelled.fulfill()
            } catch {
                XCTFail("Expected cancellation, got \(error)")
            }
        }
        await fulfillment(of: [requested], timeout: 2)
        first.cancel()
        await fulfillment(of: [cancelled], timeout: 2)
        XCTAssertTrue(bridgeCommands.isEmpty)

        let second = Task { try await account.handOver() }
        release.signal()
        await first.value
        try await second.value
        XCTAssertEqual(lock.withLock { requests.filter { $0.hasSuffix("/token") }.count }, 1)
        XCTAssertEqual(vault.values[alice.uuid]?.refresh, "fixture-rotated")
        XCTAssertEqual(bridgeCommands.count, 1)
    }

    func testFreshSignInSupersedesAStalledRefreshOfTheSameAccount() async throws {
        try seed()
        network()
        let account = model()
        let refreshing = expectation(description: "older refresh is stalled")
        let signedIn = expectation(description: "fresh sign-in selected the same account")
        let cancelled = expectation(description: "old Play stopped waiting")
        var firstTokenRequest = true
        let gate = NSLock()
        AuthURLProtocol.stallRequest = { request in
            guard request.url?.path.hasSuffix("/token") == true else { return false }
            return gate.withLock {
                guard firstTokenRequest else { return false }
                firstTokenRequest = false
                refreshing.fulfill()
                return true
            }
        }
        let response = AuthURLProtocol.response
        AuthURLProtocol.response = { request in
            if request.url?.path.hasSuffix("/devicecode") == true {
                return (200, ["device_code": "fixture-device", "user_code": "FIXTURE",
                              "verification_uri": "https://microsoft.com/link", "expires_in": 900, "interval": 1])
            }
            return response(request)
        }
        let oldPlay = Task {
            do {
                try await account.handOver()
                XCTFail("The old Play request must be cancelled")
            } catch is CancellationError {
                cancelled.fulfill()
            } catch {
                XCTFail("Expected cancellation, got \(error)")
            }
        }
        defer { oldPlay.cancel(); account.cancel() }
        await fulfillment(of: [refreshing], timeout: 2)
        let profile = account.$profile.sink { selected in
            if selected?.name == "AliceRenewed" { signedIn.fulfill() }
        }
        defer { profile.cancel() }
        account.signIn()
        await fulfillment(of: [signedIn, cancelled], timeout: 3)

        XCTAssertEqual(account.profile?.uuid, alice.uuid)
        XCTAssertEqual(vault.values[alice.uuid]?.token, "fixture-new-access")
        XCTAssertTrue(bridgeCommands.isEmpty)
        await oldPlay.value
    }

    func testFailedRefreshIsCancelledIfThePlayerChangedAccounts() async throws {
        try seed()
        network(tokenError: true)
        let account = model()
        let requested = expectation(description: "old account refresh requested")
        let release = DispatchSemaphore(value: 0)
        let response = AuthURLProtocol.response
        AuthURLProtocol.response = { request in
            if request.url!.path.hasSuffix("/token") {
                requested.fulfill()
                _ = release.wait(timeout: .now() + 5)
            }
            return response(request)
        }
        let pending = Task { try await account.handOver() }
        await fulfillment(of: [requested], timeout: 2)
        account.select(bob)
        release.signal()
        do {
            try await pending.value
            XCTFail("The old handoff should be cancelled")
        } catch is CancellationError {}
        XCTAssertEqual(account.profile, bob)
        XCTAssertEqual(account.state, .idle)
        XCTAssertTrue(bridgeCommands.isEmpty)
    }

    func testRemoveSelectedAccountIsExplicitOfflineAndKeepsOthers() async throws {
        try seed(expired: false); let account = model()
        await account.remove(alice)
        XCTAssertNil(account.profile); XCTAssertNil(vault.values[alice.uuid])
        XCTAssertNotNil(vault.values[bob.uuid]); XCTAssertEqual(account.accounts, [bob])
        XCTAssertEqual(bridgeCommands, ["launcher-session off"])
    }

    func testLateOfflineReplyDoesNotReportAnErrorOnTheNewSelection() async throws {
        for remove in [false, true] {
            try seed(expired: false)
            let requested = expectation(description: "offline session requested")
            var reply: CheckedContinuation<[String: Any], Never>?
            let account = MicrosoftAccount(defaults: defaults, vault: vault, session: session,
                                           clock: { self.instant }, client: "fixture-client") { command in
                XCTAssertEqual(command, "launcher-session off")
                return await withCheckedContinuation {
                    reply = $0
                    requested.fulfill()
                }
            }
            let pending = Task {
                if remove { await account.remove(alice) }
                else { await account.signOut() }
            }
            await fulfillment(of: [requested], timeout: 2)
            account.select(bob)
            reply?.resume(returning: ["error": "Old offline request failed"])
            await pending.value
            XCTAssertEqual(account.profile, bob)
            XCTAssertEqual(account.state, .idle)
        }
    }

    func testOldOfflineHandoffCannotClearAnActiveSignIn() async throws {
        try seed(expired: false)
        let requested = expectation(description: "offline session requested")
        let deviceRequested = expectation(description: "sign-in requested a code")
        let releaseDevice = DispatchSemaphore(value: 0)
        defer { releaseDevice.signal() }
        AuthURLProtocol.response = { _ in
            deviceRequested.fulfill()
            _ = releaseDevice.wait(timeout: .now() + 5)
            return (503, [:])
        }
        var reply: CheckedContinuation<[String: Any], Never>?
        let account = MicrosoftAccount(defaults: defaults, vault: vault, session: session,
                                       clock: { self.instant }, client: "fixture-client") { _ in
            await withCheckedContinuation {
                reply = $0
                requested.fulfill()
            }
        }
        let pending = Task { await account.signOut() }
        await fulfillment(of: [requested], timeout: 2)
        account.signIn()
        await fulfillment(of: [deviceRequested], timeout: 2)
        reply?.resume(returning: ["ok": true])
        await pending.value
        XCTAssertEqual(account.state, .working("Asking Microsoft for a code…"))
        account.cancel()
    }

    func testOldHandoffCannotPublishAfterANewerSignInEnds() async throws {
        for cancelSignIn in [true, false] {
            try seed(expired: false)
            AuthURLProtocol.response = { _ in (503, [:]) }
            let requested = expectation(description: "older handoff submitted")
            var reply: CheckedContinuation<[String: Any], Never>?
            let account = MicrosoftAccount(defaults: defaults, vault: vault, session: session,
                                           clock: { self.instant }, client: "fixture-client") { _ in
                await withCheckedContinuation {
                    reply = $0
                    requested.fulfill()
                }
            }
            let pending = Task { try await account.handOver() }
            await fulfillment(of: [requested], timeout: 2)
            account.signIn()
            if cancelSignIn {
                account.cancel()
            } else {
                for _ in 0..<100 {
                    if case .failed = account.state { break }
                    try await Task.sleep(for: .milliseconds(10))
                }
                if case .failed = account.state {} else { XCTFail("Expected sign-in failure") }
            }
            let newerState = account.state
            reply?.resume(returning: cancelSignIn ? ["error": "Old handoff failed"] : ["ok": true])
            do {
                try await pending.value
                XCTFail("The superseded handoff must not authorize Play")
            } catch is CancellationError {
                XCTAssertEqual(account.state, newerState)
                XCTAssertEqual(account.profile, alice)
            }
        }
    }

    func testFailedRemovalDoesNotDiscardAnInFlightTokenRotation() async throws {
        try seed()
        network()
        let account = model()
        let requested = expectation(description: "refresh reached Microsoft")
        let release = DispatchSemaphore(value: 0)
        let response = AuthURLProtocol.response
        AuthURLProtocol.response = { request in
            if request.url!.path.hasSuffix("/token") {
                requested.fulfill()
                _ = release.wait(timeout: .now() + 5)
            }
            return response(request)
        }
        let refresh = Task { try await account.handOver() }
        await fulfillment(of: [requested], timeout: 3)
        vault.refusedRemovals = [alice.uuid]
        await account.remove(alice)
        XCTAssertEqual(account.state, .failed("Fixture removal failure"))
        XCTAssertEqual(account.profile, alice)
        release.signal()

        try await refresh.value
        XCTAssertEqual(vault.values[alice.uuid]?.refresh, "fixture-rotated")
        XCTAssertEqual(vault.values[alice.uuid]?.token, "fixture-new-access")
        XCTAssertEqual(bridgeCommands.count, 1)
        XCTAssertTrue(bridgeCommands.last?.contains("fixture-new-access") == true)
    }
    func testFailedAddKeepsExistingSelection() async throws {
        try seed(expired: false); network(downstreamError: true)
        let response = AuthURLProtocol.response
        AuthURLProtocol.response = { request in
            if request.url!.path.hasSuffix("/devicecode") {
                return (200, ["device_code": "fixture-device", "user_code": "FIXTURE", "verification_uri": "https://microsoft.com/link", "expires_in": 900, "interval": 1])
            }
            return response(request)
        }
        let account = model(); account.signIn()
        for _ in 0..<100 {
            if case .failed = account.state { break }
            try await Task.sleep(nanoseconds: 10_000_000)
        }
        guard case .failed = account.state else { return XCTFail("fixture add did not fail") }
        XCTAssertEqual(account.profile, alice); XCTAssertEqual(account.accounts, [alice, bob])
        XCTAssertEqual(vault.values[alice.uuid]?.refresh, "fixture-old-refresh")
    }
    func testDeviceSignInRespectsLongIntervalsAndEverySlowDown() async throws {
        for initial in [60, 120] {
            AuthURLProtocol.response = { request in
                if request.url!.path.hasSuffix("/devicecode") {
                    return (200, ["device_code": "fixture-device", "user_code": "FIXTURE", "verification_uri": "https://microsoft.com/link", "expires_in": 900, "interval": initial])
                }
                return (400, ["error": "slow_down"])
            }
            var waits: [UInt64] = []
            let observed = expectation(description: "three poll delays at \(initial) seconds")
            let account = MicrosoftAccount(defaults: defaults, vault: vault, session: session, clock: {self.instant}, sleep: { delay in
                waits.append(delay)
                if waits.count == 3 {observed.fulfill(); throw CancellationError()}
            }, client: "fixture-client")
            account.signIn()
            await fulfillment(of: [observed], timeout: 5)
            account.cancel()
            XCTAssertEqual(waits, [initial, initial + 5, initial + 10].map {UInt64($0) * 1_000_000_000})
        }
    }
    func testDevicePollingBacksOffAfterTimeoutsAndStopsWhenDeclined() async {
        AuthURLProtocol.response = { request in
            if request.url!.path.hasSuffix("/devicecode") {
                return (200, ["device_code": "fixture-device", "user_code": "FIXTURE", "verification_uri": "https://microsoft.com/link", "expires_in": 900, "interval": 5])
            }
            return (400, ["error": "access_denied"])
        }
        var waits: [UInt64] = []
        let finished = expectation(description: "device sign-in declined")
        let account = MicrosoftAccount(defaults: defaults, vault: vault, session: session, clock: { self.instant }, sleep: { delay in
            waits.append(delay)
            AuthURLProtocol.failure = waits.count < 3 ? URLError(.timedOut) : nil
        }, client: "fixture-client")
        let observation = account.$state.sink { state in
            if case .failed = state { finished.fulfill() }
        }
        defer { observation.cancel(); account.cancel(); AuthURLProtocol.failure = nil }
        account.signIn()
        await fulfillment(of: [finished], timeout: 5)
        XCTAssertEqual(waits, [5, 10, 20].map { UInt64($0) * 1_000_000_000 })
        XCTAssertEqual(account.state, .failed("The sign-in was declined."))
    }

    func testDevicePollingKeepsTheCodeThroughTemporaryHTTPFailures() async {
        var replies: [(Int, [String: Any])] = [
            (503, [:]), (503, ["error": "expired_token"]),
            (429, ["error": "access_denied"]), (400, ["error": "access_denied"]),
        ]
        let lock = NSLock()
        AuthURLProtocol.response = { request in
            if request.url!.path.hasSuffix("/devicecode") {
                return (200, ["device_code": "fixture-device", "user_code": "FIXTURE",
                              "verification_uri": "https://microsoft.com/link", "expires_in": 900, "interval": 5])
            }
            return lock.withLock { replies.isEmpty ? (500, [:]) : replies.removeFirst() }
        }
        var waits: [UInt64] = []
        var clock = instant
        var deviceTime = ContinuousClock.now
        let account = MicrosoftAccount(defaults: defaults, vault: vault, session: session, clock: { clock }, deviceClock: { deviceTime }, sleep: { delay in
            waits.append(delay)
            clock.addTimeInterval(Double(delay) / 1_000_000_000)
            deviceTime = deviceTime.advanced(by: .nanoseconds(Int64(delay)))
        }, client: "fixture-client")
        let finished = expectation(description: "device sign-in declined")
        let observation = account.$state.sink { state in
            if case .failed = state { finished.fulfill() }
        }
        defer { observation.cancel(); account.cancel() }

        account.signIn()
        await fulfillment(of: [finished], timeout: 5)
        XCTAssertEqual(waits, [5, 10, 20, 40].map { UInt64($0) * 1_000_000_000 })
        XCTAssertEqual(account.state, .failed("The sign-in was declined."))
    }

    func testDevicePollingHonorsRetryAfterWithoutExtendingTheCode() async {
        AuthURLProtocol.retryAfter = { $0 == 429 ? "120" : nil }
        var clock = instant
        var deviceTime = ContinuousClock.now
        var waits: [UInt64] = []
        AuthURLProtocol.response = { request in
            if request.url!.path.hasSuffix("/devicecode") {
                return (200, ["device_code": "fixture-device", "user_code": "FIXTURE",
                              "verification_uri": "https://example.invalid", "expires_in": 40, "interval": 5])
            }
            return (429, ["error": "temporarily_unavailable"])
        }
        let account = MicrosoftAccount(defaults: defaults, vault: vault, session: session, clock: { clock }, deviceClock: { deviceTime }, sleep: { delay in
            waits.append(delay)
            clock.addTimeInterval(Double(delay) / 1_000_000_000)
            deviceTime = deviceTime.advanced(by: .nanoseconds(Int64(delay)))
        }, client: "fixture-client")
        let finished = expectation(description: "device sign-in expired")
        let observation = account.$state.sink { state in
            if case .failed = state { finished.fulfill() }
        }
        defer { observation.cancel(); account.cancel() }
        account.signIn()
        await fulfillment(of: [finished], timeout: 5)
        XCTAssertEqual(waits, [5, 35].map { UInt64($0) * 1_000_000_000 })
        XCTAssertEqual(account.state, .failed("The code expired. Try again."))
    }

    func testRetryAfterAcceptsSecondsAndHTTPDates() {
        let now = Date(timeIntervalSince1970: 0)
        XCTAssertEqual(AccountNet.retryDelay("90", now: now), 90)
        for date in ["Thu, 01 Jan 1970 00:01:30 GMT", "Thursday, 01-Jan-70 00:01:30 GMT", "Thu Jan  1 00:01:30 1970"] {
            XCTAssertEqual(AccountNet.retryDelay(date, now: now), 90)
        }
        XCTAssertEqual(AccountNet.retryDelay("invalid", now: now), 0)
    }

    func testDevicePollCancelsAStalledRequestAtTheCodeDeadline() async throws {
        AuthURLProtocol.stall = true
        defer { AuthURLProtocol.stall = false }
        let net = AccountNet(session: session)
        do {
            _ = try await net.form("https://example.invalid/token", [:], allowErrors: true,
                                   deadline: ContinuousClock.now.advanced(by: .milliseconds(100)))
            XCTFail("The stalled request should have expired")
        } catch {
            XCTAssertEqual(error.localizedDescription, "The code expired. Try again.")
        }
    }

    func testDeviceCodeExpiryIgnoresWallClockChanges() async {
        for jump in [-3_600.0, 3_600.0] {
            var polls = 0
            let lock = NSLock()
            AuthURLProtocol.response = { request in
                if request.url!.path.hasSuffix("/devicecode") {
                    return (200, ["device_code": "fixture-device", "user_code": "FIXTURE",
                                  "verification_uri": "https://example.invalid", "expires_in": 10, "interval": 5])
                }
                return lock.withLock {
                    polls += 1
                    return (400, ["error": polls == 1 ? "authorization_pending" : "access_denied"])
                }
            }
            var wall = instant
            var deviceTime = ContinuousClock.now
            var waits: [UInt64] = []
            let account = MicrosoftAccount(defaults: defaults, vault: vault, session: session,
                                           clock: { wall }, deviceClock: { deviceTime }, sleep: { delay in
                waits.append(delay)
                wall.addTimeInterval(waits.count == 1 ? jump : Double(delay) / 1_000_000_000)
                deviceTime = deviceTime.advanced(by: .nanoseconds(Int64(delay)))
            }, client: "fixture-client")
            let finished = expectation(description: "device code expired")
            let observation = account.$state.sink { state in
                if case .failed = state { finished.fulfill() }
            }
            account.signIn()
            await fulfillment(of: [finished], timeout: 5)
            observation.cancel()
            XCTAssertEqual(account.state, .failed("The code expired. Try again."))
            XCTAssertEqual(lock.withLock { polls }, 1)
            XCTAssertEqual(waits, [5_000_000_000, 5_000_000_000])
            account.cancel()
        }
    }

    func testDeviceSignInStopsIfTheCodeExpiresWhileWaiting() async throws {
        var tokenRequests = 0
        AuthURLProtocol.response = { request in
            if request.url!.path.hasSuffix("/devicecode") {
                return (200, ["device_code": "fixture-device", "user_code": "FIXTURE", "verification_uri": "https://microsoft.com/link", "expires_in": 1, "interval": 5])
            }
            self.lock.withLock {tokenRequests += 1}
            return (400, ["error": "expired_token"])
        }
        var clock = instant
        var deviceTime = ContinuousClock.now
        let account = MicrosoftAccount(defaults: defaults, vault: vault, session: session, clock: { clock }, deviceClock: { deviceTime }, sleep: { delay in
            clock.addTimeInterval(Double(delay) / 1_000_000_000)
            deviceTime = deviceTime.advanced(by: .nanoseconds(Int64(delay)))
        }, client: "fixture-client")
        let finished = expectation(description: "expired code ends sign-in")
        let observation = account.$state.sink {state in
            if case .failed = state {finished.fulfill()}
        }
        account.signIn()
        await fulfillment(of: [finished], timeout: 5)
        observation.cancel(); account.cancel()
        let count = lock.withLock {tokenRequests}
        XCTAssertEqual(count, 0, "An expired device code must never be polled")
    }
    func testCancelAddCannotReplaceSelection() async throws {
        try seed(expired: false); network()
        let codeRequested = expectation(description: "device code requested")
        AuthURLProtocol.response = { _ in
            codeRequested.fulfill()
            return (200, ["device_code": "fixture-device", "user_code": "FIXTURE", "verification_uri": "https://microsoft.com/link", "expires_in": 900, "interval": 1])
        }
        let account = MicrosoftAccount(defaults: defaults, vault: vault, session: session, clock: { self.instant }, sleep: { _ in try await Task.sleep(nanoseconds: 5_000_000_000) }, client: "fixture")
        account.signIn(); await fulfillment(of: [codeRequested], timeout: 3)
        account.cancel(); await Task.yield()
        XCTAssertEqual(account.profile, alice); XCTAssertEqual(account.state, .idle)
        XCTAssertEqual(account.accounts, [alice, bob])
    }
    func testInstalledGameFilesRetryRestartsLoadingWithoutLookingUpADownload() async throws {
        var commands: [String] = []
        var state: [String: Any] = ["phase": "needs_files", "retry_loader": true]
        let game = GameLink(stopAudio: {}) { command in
            commands.append(command)
            if command == "launcher-retry-loader" {
                state = ["phase": "loading"]
                return ["ok": true]
            }
            return state
        }
        await game.refresh()
        let retry = try XCTUnwrap(game.retryInstalledFiles)
        await retry()
        await retry()
        XCTAssertEqual(commands, ["launcher-state", "launcher-retry-loader", "launcher-state"])
        XCTAssertEqual(game.phase, .loading(0))
        XCTAssertNil(game.retryInstalledFiles)
    }

    func testOlderServerListCannotReplaceTheNewestList() async {
        let requested = expectation(description: "old server list requested")
        var oldReply: CheckedContinuation<[String: Any], Never>?
        var listRequests = 0
        let game = GameLink(send: { command in
            XCTAssertEqual(command, "servers")
            listRequests += 1
            if listRequests == 1 {
                return await withCheckedContinuation {
                    oldReply = $0
                    requested.fulfill()
                }
            }
            return ["servers": [["name": "Current", "address": "current.invalid"]]]
        })
        let old = Task { await game.reloadServers() }
        await fulfillment(of: [requested], timeout: 2)
        await game.reloadServers()
        oldReply?.resume(returning: ["servers": [["name": "Old", "address": "old.invalid"]]])
        await old.value

        XCTAssertEqual(game.servers.map(\.address), ["current.invalid"])
        XCTAssertEqual(game.serverRound, 1)
    }

    func testCancelLaunchWhilePreparingDoesNotUncover() async throws {
        let game = GameLink(); var uncovered = false
        var preparationCancelled = false
        let started = expectation(description: "preparing")
        var continuation: CheckedContinuation<Void, Never>?
        game.prepareSession = {
            started.fulfill()
            await withCheckedContinuation { continuation = $0 }
            preparationCancelled = Task.isCancelled
        }
        game.uncover = { uncovered = true }
        let play = Task { await game.play() }
        await fulfillment(of: [started], timeout: 3)
        game.cancelLaunch(); continuation?.resume(); await play.value
        XCTAssertTrue(preparationCancelled)
        XCTAssertFalse(uncovered); XCTAssertEqual(game.phase, .unknown)
    }

    func testCancelDuringAccountHandoffDoesNotReportALateBridgeFailure() async throws {
        try seed(expired: false)
        let submitted = expectation(description: "account handoff submitted")
        var reply: CheckedContinuation<[String: Any], Never>?
        let account = MicrosoftAccount(defaults: defaults, vault: vault, session: session,
                                       clock: { self.instant }, client: "fixture-client") { _ in
            await withCheckedContinuation {
                reply = $0
                submitted.fulfill()
            }
        }
        let handoff = Task { try await account.handOver() }
        await fulfillment(of: [submitted], timeout: 3)
        handoff.cancel()
        reply?.resume(returning: ["error": "late bridge failure"])
        do {
            try await handoff.value
            XCTFail("the cancelled handoff should throw")
        } catch is CancellationError {
            XCTAssertEqual(account.state, .idle)
        }
    }

    func testSuccessfulAddPersistsSeparateCredentialAndCanSwitchBack() async throws {
        try seed(expired: false); network()
        let response = AuthURLProtocol.response
        let third = String(repeating: "3", count: 32)
        AuthURLProtocol.response = { request in
            if request.url!.path.hasSuffix("/devicecode") {
                return (200, ["device_code": "fixture-device", "user_code": "FIXTURE", "verification_uri": "https://microsoft.com/link", "expires_in": 900, "interval": 1])
            }
            if request.url!.path == "/minecraft/profile" { return (200, ["id": third, "name": "Third"]) }
            return response(request)
        }
        let account = model(); account.signIn()
        for _ in 0..<100 {
            if account.profile?.uuid == third { break }
            try await Task.sleep(nanoseconds: 10_000_000)
        }
        XCTAssertEqual(account.profile?.uuid, third); XCTAssertEqual(account.accounts.count, 3)
        XCTAssertNotNil(vault.values[third]); XCTAssertNotNil(vault.values[alice.uuid])
        account.select(alice); try await account.handOver()
        XCTAssertTrue(bridgeCommands.last?.contains("fixture-old-access") == true)
        XCTAssertEqual(model().profile, alice)
    }

    func testNetworkFailurePreservesCredentialsAndDoesNotRequestReauthentication() async throws {
        try seed(); AuthURLProtocol.failure = URLError(.notConnectedToInternet)
        let account = model()
        do { try await account.handOver(); XCTFail("must fail") } catch {
            XCTAssertFalse((error as? AccountError)?.reauthenticate ?? true)
        }
        XCTAssertEqual(vault.values[alice.uuid]?.refresh, "fixture-old-refresh")
        XCTAssertEqual(account.profile, alice); XCTAssertTrue(bridgeCommands.isEmpty)
    }
    func testFailedRotationStorageStopsBeforeXbox() async throws {
        try seed(); network(); vault.failWrites = true
        let account = model()
        do { try await account.handOver(); XCTFail("must fail") } catch {}
        XCTAssertEqual(vault.values[alice.uuid]?.refresh, "fixture-old-refresh")
        XCTAssertEqual(requests.count, 1); XCTAssertTrue(bridgeCommands.isEmpty)
    }

    func testRemovingLegacyAccountDuringDeferredMigrationCannotRestoreIt() async throws {
        defaults.set(try JSONEncoder().encode(alice), forKey: "microsoftProfile")
        vault.values["account"] = AccountSecrets(refresh: "fixture-legacy", token: "fixture-valid", expires: instant.addingTimeInterval(7200))
        vault.failWrites = true
        let account = model()
        await account.remove(alice)
        vault.failWrites = false
        try await account.handOver()
        XCTAssertTrue(vault.values.isEmpty); XCTAssertTrue(account.accounts.isEmpty)
        XCTAssertNil(defaults.data(forKey: "microsoftProfile")); XCTAssertNil(account.profile)
    }

    func testFailedLegacyRemovalKeepsTheNewerCredentialUntilRemovalCanRetry() async throws {
        try seed(expired: false)
        defaults.set(try JSONEncoder().encode(alice), forKey: "microsoftProfile")
        let current = try XCTUnwrap(vault.values[alice.uuid])
        vault.values["account"] = AccountSecrets(refresh: "fixture-stale-refresh", token: "fixture-stale-access",
                                                 expires: instant)
        vault.refusedRemovals = ["account"]
        let account = model()
        await account.remove(alice)

        XCTAssertEqual(account.profile, alice)
        XCTAssertEqual(vault.values[alice.uuid], current)
        XCTAssertNotNil(vault.values["account"])
        XCTAssertTrue(bridgeCommands.isEmpty)
        XCTAssertEqual(account.state, .failed("Fixture removal failure"))

        vault.refusedRemovals = []
        let relaunched = model()
        try await relaunched.handOver()
        XCTAssertTrue(bridgeCommands.last?.contains(current.token) == true)
        XCTAssertNil(vault.values["account"])
        await relaunched.remove(alice)
        XCTAssertNil(vault.values[alice.uuid])
        XCTAssertEqual(relaunched.accounts, [bob])
        XCTAssertNil(relaunched.profile)
        XCTAssertEqual(bridgeCommands.last, "launcher-session off")
    }

    func testRemovingAnotherAccountKeepsUnresolvedSelectionAcrossRelaunch() async throws {
        try seed(expired: false)
        defaults.set(try JSONEncoder().encode(MicrosoftAccount.Catalog(
            profiles: [alice, bob], selected: "missing"
        )), forKey: "microsoftAccounts.v2")
        let account = model()
        XCTAssertTrue(account.needsAccountSelection)

        await account.remove(bob)
        let relaunched = model()
        XCTAssertEqual(relaunched.accounts, [alice])
        XCTAssertTrue(relaunched.needsAccountSelection)
        relaunched.select(bob)
        XCTAssertTrue(relaunched.needsAccountSelection)
        XCTAssertNil(relaunched.profile)
        XCTAssertTrue(bridgeCommands.isEmpty)
    }

    func testUnreadableCatalogRequiresExplicitOfflineChoice() async throws {
        let unreadable = Data("unreadable accounts".utf8)
        defaults.set(unreadable, forKey: "microsoftAccounts.v2")
        let account = model()
        XCTAssertTrue(account.needsAccountSelection)
        do {
            try await account.handOver()
            XCTFail("Unreadable account metadata must not silently launch Offline")
        } catch {}
        XCTAssertTrue(bridgeCommands.isEmpty)
        XCTAssertEqual(defaults.data(forKey: "microsoftAccounts.v2"), unreadable)

        await account.signOut()
        XCTAssertFalse(model().needsAccountSelection)
        XCTAssertEqual(bridgeCommands, ["launcher-session off"])
    }

    func testSuccessfulSignInRepairsMissingSelectedAccount() async throws {
        try seed(expired: false)
        defaults.set(try JSONEncoder().encode(MicrosoftAccount.Catalog(profiles: [alice], selected: "missing")), forKey: "microsoftAccounts.v2")
        network(); let response = AuthURLProtocol.response
        AuthURLProtocol.response = { request in
            if request.url!.path.hasSuffix("/devicecode") {
                return (200, ["device_code": "fixture-device", "user_code": "FIXTURE", "verification_uri": "https://microsoft.com/link", "expires_in": 900, "interval": 1])
            }
            return response(request)
        }
        let account = model(); XCTAssertTrue(account.needsAccountSelection)
        account.signIn()
        for _ in 0..<100 {
            if account.profile != nil { break }
            try await Task.sleep(nanoseconds: 10_000_000)
        }
        XCTAssertFalse(account.needsAccountSelection)
        try await account.handOver()
        XCTAssertTrue(bridgeCommands.last?.contains("fixture-new-access") == true)
    }
    func testCancelledJoinReplyCannotUncoverOrClearNewLaunch() async throws {
        preparedGameFiles = try PreparedGameFilesFixture()
        let connectStarted = expectation(description: "connect started")
        var connection: CheckedContinuation<[String: Any], Never>?
        let game = GameLink { command in
            if command == "launcher-state" { return ["phase": "title"] }
            if command.hasPrefix("launcher-start ") { return ["ok": true, "phase": "title"] }
            if command.hasPrefix("launcher-commit-start ") { return ["ok": true, "status": "committed"] }
            if command.hasPrefix("launcher-cancel-start ") { return ["ok": true, "status": "stopped"] }
            if command.hasPrefix("connect ") {
                connectStarted.fulfill()
                return await withCheckedContinuation { connection = $0 }
            }
            return ["ok": true]
        }
        await game.refresh(); game.prepareSession = {}
        var uncovered = false; game.uncover = { uncovered = true }
        let join = Task { await game.join("fixture.invalid") }
        await fulfillment(of: [connectStarted], timeout: 3)
        game.cancelLaunch()
        let deadline = Date().addingTimeInterval(3)
        while game.launching != nil, Date() < deadline { await Task.yield() }
        XCTAssertNil(game.launching, "The canceled start must drain before a replacement begins")
        let preparing = expectation(description: "new launch preparing")
        var preparation: CheckedContinuation<Void, Never>?
        game.prepareSession = {
            preparing.fulfill()
            await withCheckedContinuation { preparation = $0 }
        }
        let newLaunch = Task { await game.play() }
        await fulfillment(of: [preparing], timeout: 3)
        connection?.resume(returning: ["ok": true]); await join.value
        XCTAssertFalse(uncovered)
        XCTAssertEqual(game.launching, "Preparing account…")
        game.cancelLaunch(); preparation?.resume(); await newLaunch.value
        XCTAssertFalse(uncovered); XCTAssertNil(game.launching)
    }

}

@MainActor
final class LaunchCancellationTests: XCTestCase {
    private var preparedGameFiles: PreparedGameFilesFixture?

    override func setUp() async throws {
        // These tests start/cancel a prepared game. GameLink deliberately refuses
        // a waiting engine when its resource-ready marker is absent.
        preparedGameFiles = try PreparedGameFilesFixture()
        XCTAssertTrue(GameFolder.hasGameFiles, "The launch lifecycle fixture requires prepared resources")
    }

    override func tearDown() async throws {
        try preparedGameFiles?.cleanUp()
        preparedGameFiles = nil
    }

    @MainActor
    private final class Engine {
        var phase = "waiting"
        var owner: String?
        var committed = false
        var pendingCancel: String?
        var stoppedOwner: String?
        var commands: [String] = []
        var starts: [String] = []
        var commits: [String] = []
        var cancels: [String] = []
        var cancellationTaskFlags: [Bool] = []
        var onStart: ((String) -> Void)?
        var onCommit: ((String) -> Void)?
        var onCancel: ((String) -> Void)?
        var holdStart = true
        var startReplyError: String?
        var holdCommit = false
        var holdCancel = false
        var holdCommittedCancelReply = false
        var cancelFailures = 0
        var startReplies: [String: (String, CheckedContinuation<[String: Any], Never>)] = [:]
        var commitReplies: [String: CheckedContinuation<[String: Any], Never>] = [:]
        var cancelReplies: [String: CheckedContinuation<Void, Never>] = [:]
        var running: Bool { phase != "waiting" }

        func send(_ command: String) async -> [String: Any] {
            commands.append(command)
            if command == "launcher-state" {
                var status: [String: Any] = ["phase": phase]
                status["stopped_owner"] = stoppedOwner
                return status
            }
            if command.hasPrefix("launcher-start ") {
                let id = String(command.dropFirst("launcher-start ".count))
                guard pendingCancel == nil else { return ["error": "Cancellation is still draining"] }
                owner = id
                committed = committed || phase == "world"
                if phase == "waiting" { phase = "loading" }
                let replyPhase = phase
                starts.append(id)
                onStart?(id)
                if holdStart {
                    return await withCheckedContinuation { startReplies[id] = (replyPhase, $0) }
                }
                if let startReplyError { return ["error": startReplyError] }
                return ["ok": true, "phase": replyPhase]
            }
            if command.hasPrefix("launcher-commit-start ") {
                let id = String(command.dropFirst("launcher-commit-start ".count))
                guard owner == id, pendingCancel == nil, running else {
                    return ["ok": false, "error": "Start no longer active"]
                }
                // The backend commits before replying, independently of when
                // the caller's actor continuation receives the acknowledgment.
                committed = true
                commits.append(id)
                onCommit?(id)
                if holdCommit {
                    return await withCheckedContinuation { commitReplies[id] = $0 }
                }
                return ["ok": true, "status": "committed"]
            }
            if command.hasPrefix("launcher-cancel-start ") {
                let id = String(command.dropFirst("launcher-cancel-start ".count))
                cancels.append(id)
                cancellationTaskFlags.append(Task.isCancelled)
                onCancel?(id)
                guard owner == id else { return ["ok": true, "status": "stale"] }
                guard !committed else {
                    if holdCommittedCancelReply {
                        await withCheckedContinuation { cancelReplies[id] = $0 }
                    }
                    return ["ok": true, "status": "committed"]
                }
                pendingCancel = id
                if cancelFailures > 0 {
                    cancelFailures -= 1
                    return ["error": "Fixture transport failure"]
                }
                if holdCancel {
                    await withCheckedContinuation { cancelReplies[id] = $0 }
                }
                guard owner == id, pendingCancel == id else { return ["ok": true, "status": "stale"] }
                reset()
                return ["ok": true, "status": "stopped"]
            }
            return ["error": "Unexpected fixture command"]
        }

        func reset() {
            phase = "waiting"
            owner = nil
            pendingCancel = nil
            committed = false
        }

        func releaseStart(_ id: String) {
            guard let (phase, reply) = startReplies.removeValue(forKey: id) else { return }
            reply.resume(returning: ["ok": true, "phase": phase])
        }
        func releaseCommit(_ id: String) {
            commitReplies.removeValue(forKey: id)?.resume(returning: ["ok": true, "status": "committed"])
        }
        func releaseCancel(_ id: String) { cancelReplies.removeValue(forKey: id)?.resume() }
    }
    @MainActor
    private final class Presentation { var count = 0 }

    private func game(_ engine: Engine, _ shown: Presentation,
                      stopAudio: @escaping () -> Void = {}) async -> GameLink {
        let game = GameLink(stopAudio: stopAudio) { await engine.send($0) }
        game.prepareSession = {}
        game.uncover = { shown.count += 1 }
        await game.refresh()
        return game
    }
    private func heldStart(_ engine: Engine, _ game: GameLink) async -> Task<Void, Never> {
        let accepted = expectation(description: "Backend accepted Play")
        engine.onStart = { _ in accepted.fulfill(); engine.onStart = nil }
        let play = Task { await game.play() }
        await fulfillment(of: [accepted], timeout: 3)
        return play
    }
    private func until(_ condition: @MainActor () -> Bool, _ message: String = "State did not settle") async {
        let deadline = Date().addingTimeInterval(3)
        while !condition(), Date() < deadline { await Task.yield() }
        XCTAssertTrue(condition(), message)
    }

    func testCancelWhileStartAcknowledgmentIsPendingStopsHiddenEngine() async {
        let engine = Engine(), shown = Presentation()
        var audioStops = 0
        let game = await game(engine, shown) { audioStops += 1 }
        let play = await heldStart(engine, game)
        XCTAssertEqual(game.phase, .waiting, "The start acknowledgement is still pending")
        let audioStopsBeforeCancel = audioStops
        let id = engine.starts[0]
        XCTAssertNotNil(game.launching, "The launcher still exposes Cancel during this await")
        XCTAssertTrue(engine.running)
        game.cancelLaunch()
        engine.releaseStart(id)
        await play.value
        await until({ game.launching == nil })
        XCTAssertEqual(shown.count, 0)
        XCTAssertFalse(engine.running)
        XCTAssertTrue(engine.commits.isEmpty, "Cancellation won before presentation commit")
        XCTAssertEqual(audioStops, audioStopsBeforeCancel + 1)
    }

    func testUnconfirmedStartDrainsOwnedEngineBeforeEnablingPlayAgain() async {
        let engine = Engine(), shown = Presentation()
        engine.holdStart = false
        engine.holdCancel = true
        engine.startReplyError = "Unusable start reply"
        let game = await game(engine, shown)
        let play = Task { await game.play() }
        await until({ engine.pendingCancel != nil }, "The ambiguous start must request shutdown")
        guard let id = engine.pendingCancel else {
            await play.value
            return
        }
        XCTAssertTrue(engine.running)
        XCTAssertNotNil(game.launching)
        await game.play()
        XCTAssertEqual(engine.starts, [id])
        XCTAssertEqual(shown.count, 0)

        engine.releaseCancel(id)
        await play.value
        XCTAssertFalse(engine.running)
        XCTAssertNil(game.launching)
        XCTAssertEqual(game.problem, "Unusable start reply")
        XCTAssertEqual(engine.cancels, [id])
    }

    func testTaskCancellationAfterAcceptedStartCannotUncoverOrLeaveHiddenEngine() async {
        let engine = Engine(), shown = Presentation()
        let game = await game(engine, shown)
        let play = await heldStart(engine, game)
        let id = engine.starts[0]
        play.cancel()
        engine.releaseStart(id)
        await play.value
        await until({ game.launching == nil })
        XCTAssertEqual(shown.count, 0)
        XCTAssertFalse(engine.running)
    }

    func testTaskCancellationRunsCleanupWithoutWaitingForStartAcknowledgment() async {
        let engine = Engine(), shown = Presentation()
        let game = await game(engine, shown)
        let play = await heldStart(engine, game)
        let id = engine.starts[0]
        play.cancel()
        await until({ game.launching == nil }, "Cleanup was inherited by the canceled caller")
        XCTAssertFalse(engine.running)
        XCTAssertEqual(engine.cancellationTaskFlags, [false], "Cleanup must run in a non-canceled task")
        XCTAssertNotNil(engine.startReplies[id], "The original start continuation must still be suspended")
        engine.releaseStart(id)
        await play.value
        XCTAssertEqual(shown.count, 0)
    }

    private func checkCancellationBeforeStart(cancelTask: Bool) async {
        let engine = Engine(), shown = Presentation()
        let game = await game(engine, shown)
        let preparing = expectation(description: "Preparing account")
        var preparation: CheckedContinuation<Void, Never>?
        game.prepareSession = {
            preparing.fulfill()
            await withCheckedContinuation { preparation = $0 }
        }
        let play = Task { await game.play() }
        await fulfillment(of: [preparing], timeout: 3)
        if cancelTask { play.cancel() } else { game.cancelLaunch() }
        preparation?.resume()
        await play.value
        XCTAssertFalse(engine.running)
        XCTAssertTrue(engine.starts.isEmpty)
        XCTAssertTrue(engine.cancels.isEmpty)
        XCTAssertEqual(shown.count, 0)
        XCTAssertNil(game.launching)
    }
    func testCancelBeforeBackendStartLeavesEngineOff() async { await checkCancellationBeforeStart(cancelTask: false) }
    func testTaskCancellationBeforeBackendStartLeavesEngineOff() async { await checkCancellationBeforeStart(cancelTask: true) }

    func testOrdinaryPlayStartsAndUncoversEngine() async {
        let engine = Engine(), shown = Presentation()
        engine.holdStart = false
        let game = await game(engine, shown)
        await game.play()
        XCTAssertTrue(engine.running)
        XCTAssertEqual(shown.count, 1)
        XCTAssertEqual(game.phase, .loading(0))
        XCTAssertEqual(engine.starts, engine.commits)
        XCTAssertNil(game.launching)
        XCTAssertNil(game.problem)
    }

    func testCachedTitleCannotSkipStartingAResetBackend() async {
        let engine = Engine(), shown = Presentation()
        engine.phase = "title"
        engine.holdStart = false
        let game = await game(engine, shown)
        XCTAssertEqual(game.phase, .title)
        engine.phase = "waiting"
        await game.play()
        XCTAssertTrue(engine.running)
        XCTAssertEqual(engine.starts.count, 1)
        XCTAssertEqual(game.phase, .loading(0))
        XCTAssertEqual(shown.count, 1)
    }

    func testNewPlayWaitsForCancellationDrainAndCleanupIsCoalesced() async {
        let engine = Engine(), shown = Presentation()
        engine.holdCancel = true
        let game = await game(engine, shown)
        let play = await heldStart(engine, game)
        let id = engine.starts[0]
        let cancelling = expectation(description: "Cancellation accepted")
        engine.onCancel = { _ in cancelling.fulfill(); engine.onCancel = nil }
        game.cancelLaunch()
        await fulfillment(of: [cancelling], timeout: 3)
        game.cancelLaunch()
        await game.play()
        XCTAssertEqual(engine.starts.count, 1)
        XCTAssertEqual(engine.cancels, [id])
        XCTAssertNotNil(game.launching, "New Play must stay disabled until physical drain acknowledgment")
        XCTAssertTrue(engine.running)
        engine.releaseCancel(id)
        await until({ game.launching == nil })
        engine.releaseStart(id)
        await play.value
        XCTAssertFalse(engine.running)
        XCTAssertEqual(shown.count, 0)
        engine.holdStart = false
        await game.play()
        XCTAssertEqual(engine.starts.count, 2)
        XCTAssertEqual(shown.count, 1)
    }

    func testLateOldStartAcknowledgmentCannotPresentOrClearANewerLaunch() async {
        let engine = Engine(), shown = Presentation()
        engine.phase = "title"
        let game = await game(engine, shown)
        let old = await heldStart(engine, game)
        let oldID = engine.starts[0]
        game.cancelLaunch()
        await until({ game.launching == nil })
        let preparing = expectation(description: "New launch preparing")
        var preparation: CheckedContinuation<Void, Never>?
        game.prepareSession = {
            preparing.fulfill()
            await withCheckedContinuation { preparation = $0 }
        }
        let next = Task { await game.play() }
        await fulfillment(of: [preparing], timeout: 3)
        engine.releaseStart(oldID)
        await old.value
        XCTAssertEqual(shown.count, 0)
        XCTAssertEqual(game.phase, .waiting)
        XCTAssertNotNil(game.launching)
        engine.holdStart = false
        preparation?.resume()
        await next.value
        XCTAssertEqual(shown.count, 1)
        XCTAssertEqual(engine.cancels, [oldID])
        XCTAssertEqual(engine.owner, engine.starts.last)
    }

    private func checkCancellationAfterCommit(cancelTask: Bool) async {
        let engine = Engine(), shown = Presentation()
        engine.holdStart = false
        engine.holdCommit = true
        let game = await game(engine, shown)
        let committed = expectation(description: "Presentation committed before reply")
        engine.onCommit = { _ in committed.fulfill(); engine.onCommit = nil }
        let play = Task { await game.play() }
        await fulfillment(of: [committed], timeout: 3)
        let id = engine.starts[0]
        if cancelTask { play.cancel() } else { game.cancelLaunch() }
        await until({ shown.count == 1 })
        XCTAssertTrue(engine.running)
        XCTAssertNotEqual(game.phase, .waiting)
        engine.releaseCommit(id)
        await play.value
        XCTAssertEqual(shown.count, 1, "Late commit/cancel replies must not uncover twice")
        XCTAssertNil(game.launching)
    }
    func testCancelAfterPresentationCommitMustUncoverOnce() async { await checkCancellationAfterCommit(cancelTask: false) }
    func testTaskCancellationAfterPresentationCommitMustUncoverOnce() async { await checkCancellationAfterCommit(cancelTask: true) }

    func testCommittedExistingWorldIsNotStoppedByCancel() async {
        let engine = Engine(), shown = Presentation()
        engine.phase = "world"
        let game = await game(engine, shown)
        let play = await heldStart(engine, game)
        let id = engine.starts[0]
        game.cancelLaunch()
        await until({ shown.count == 1 })
        XCTAssertEqual(engine.phase, "world")
        engine.releaseStart(id)
        await play.value
        XCTAssertEqual(shown.count, 1)
    }

    func testUnprovenCleanupDoesNotEnablePlayAndCanRetry() async {
        let engine = Engine(), shown = Presentation()
        engine.cancelFailures = 1
        let game = await game(engine, shown)
        let play = await heldStart(engine, game)
        let id = engine.starts[0]
        game.cancelLaunch()
        await until({ game.problem != nil })
        XCTAssertNotNil(game.launching)
        XCTAssertTrue(engine.running)
        await game.play()
        XCTAssertEqual(engine.starts.count, 1)
        game.cancelLaunch()
        await until({ game.launching == nil })
        engine.releaseStart(id)
        await play.value
        XCTAssertFalse(engine.running)
        XCTAssertEqual(shown.count, 0)
        XCTAssertEqual(engine.cancels, [id, id])
    }
    func testMissedCancellationReplyRecoversOnlyForTheMatchingOwner() async {
        let engine = Engine(), shown = Presentation()
        engine.cancelFailures = 1
        let game = await game(engine, shown)
        let play = await heldStart(engine, game)
        let id = engine.starts[0]
        game.cancelLaunch()
        await until({ game.problem != nil })

        engine.reset()
        engine.stoppedOwner = UUID().uuidString
        await game.refresh()
        XCTAssertNotNil(game.launching)

        engine.stoppedOwner = id
        await game.refresh()
        XCTAssertNil(game.launching)
        XCTAssertNil(game.problem)
        XCTAssertEqual(game.phase, .waiting)
        XCTAssertEqual(engine.cancels, [id])
        engine.releaseStart(id)
        await play.value
        XCTAssertEqual(shown.count, 0)
    }

    func testLateCommittedStartReplyCannotPresentOrClearANewerLaunch() async {
        let engine = Engine(), shown = Presentation()
        engine.holdStart = false
        engine.holdCommit = true
        let game = await game(engine, shown)
        let committed = expectation(description: "First presentation committed")
        engine.onCommit = { _ in committed.fulfill(); engine.onCommit = nil }
        let old = Task { await game.play() }
        await fulfillment(of: [committed], timeout: 3)
        let oldID = engine.starts[0]
        game.cancelLaunch()
        await until({ shown.count == 1 })
        let preparing = expectation(description: "Replacement preparing")
        var preparation: CheckedContinuation<Void, Never>?
        game.prepareSession = {
            preparing.fulfill()
            await withCheckedContinuation { preparation = $0 }
        }
        let next = Task { await game.play() }
        await fulfillment(of: [preparing], timeout: 3)
        engine.releaseCommit(oldID)
        await old.value
        XCTAssertEqual(shown.count, 1)
        XCTAssertNotNil(game.launching)
        engine.holdCommit = false
        preparation?.resume()
        await next.value
        XCTAssertEqual(shown.count, 2)
        XCTAssertEqual(engine.cancels, [oldID])
        XCTAssertEqual(engine.owner, engine.starts.last)
    }

    func testLateCommittedCancellationReplyCannotPresentANewerLaunch() async {
        let engine = Engine(), shown = Presentation()
        engine.holdStart = false
        engine.holdCommit = true
        engine.holdCommittedCancelReply = true
        let game = await game(engine, shown)
        let committed = expectation(description: "Presentation committed")
        engine.onCommit = { _ in committed.fulfill(); engine.onCommit = nil }
        let old = Task { await game.play() }
        await fulfillment(of: [committed], timeout: 3)
        let oldID = engine.starts[0]
        game.cancelLaunch()
        await until({ engine.cancelReplies[oldID] != nil })
        engine.releaseCommit(oldID)
        await old.value
        XCTAssertEqual(shown.count, 1)
        let preparing = expectation(description: "New launch preparing")
        var preparation: CheckedContinuation<Void, Never>?
        game.prepareSession = {
            preparing.fulfill()
            await withCheckedContinuation { preparation = $0 }
        }
        let next = Task { await game.play() }
        await fulfillment(of: [preparing], timeout: 3)
        engine.releaseCancel(oldID)
        await Task.yield()
        XCTAssertEqual(shown.count, 1)
        XCTAssertNotNil(game.launching)
        engine.holdCommit = false
        preparation?.resume()
        await next.value
        XCTAssertEqual(shown.count, 2)
        XCTAssertEqual(engine.cancels, [oldID])
    }

    func testNewOwnedStartPreservesCommittedTitleAndCancelUncoversOnce() async {
        let engine = Engine(), shown = Presentation()
        engine.phase = "title"
        engine.committed = true
        let game = await game(engine, shown)
        let play = await heldStart(engine, game)
        let id = engine.starts[0]
        XCTAssertTrue(engine.committed, "A new owner must not undo an existing presentation commit")
        game.cancelLaunch()
        await until({ shown.count == 1 })
        XCTAssertTrue(engine.running)
        XCTAssertEqual(engine.phase, "title")
        engine.releaseStart(id)
        await play.value
        XCTAssertEqual(shown.count, 1)
        XCTAssertEqual(game.phase, .title)
    }

    func testCachedCommittedTitleAfterBackendResetStartsCancellableFreshEngine() async {
        let engine = Engine(), shown = Presentation()
        engine.phase = "title"
        engine.committed = true
        let game = await game(engine, shown)
        engine.reset()
        XCTAssertEqual(game.phase, .title, "The client deliberately still has its old cached phase")
        let play = await heldStart(engine, game)
        let id = engine.starts[0]
        XCTAssertFalse(engine.committed, "An explicit reset starts a fresh uncommitted generation")
        game.cancelLaunch()
        engine.releaseStart(id)
        await play.value
        await until({ game.launching == nil })
        XCTAssertFalse(engine.running)
        XCTAssertEqual(shown.count, 0)
        XCTAssertTrue(engine.commits.isEmpty)
        XCTAssertEqual(game.phase, .waiting)
    }

}
