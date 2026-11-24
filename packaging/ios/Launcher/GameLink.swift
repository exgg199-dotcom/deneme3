import Darwin
import SwiftUI
import UIKit

/// The game's `Documents`: where it keeps saves, screenshots, options.txt and the imported game files.
enum GameFolder {
    static let documents = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask).first
        ?? URL(fileURLWithPath: NSHomeDirectory()).appendingPathComponent("Documents")
    static var saves: URL { documents.appendingPathComponent("saves") }
    static var screenshots: URL { documents.appendingPathComponent("screenshots") }
    static var resources: URL { documents.appendingPathComponent("resources") }

    /// Whether imported game files are in place (`import::resources_root`'s second stop).
    static var hasGameFiles: Bool { resourceMarker(at: resources) != nil }

    static func containsGameFiles(at resources: URL) -> Bool {
        resourceMarker(at: resources) != nil
    }

    /// Keep this fingerprint in sync with the engine's import::resource_marker.
    static func resourceMarker(at resources: URL) -> String? {
        let path = resources.appendingPathComponent("horizon-resources.json").path
        var metadata = stat()
        guard fstatat(AT_FDCWD, path, &metadata, 0) == 0,
              metadata.st_mode & mode_t(S_IFMT) == mode_t(S_IFREG) else { return nil }
        return [
            UInt64(truncatingIfNeeded: metadata.st_dev).description,
            metadata.st_ino.description,
            metadata.st_size.description,
            metadata.st_mtimespec.tv_sec.description,
            metadata.st_mtimespec.tv_nsec.description,
            metadata.st_ctimespec.tv_sec.description,
            metadata.st_ctimespec.tv_nsec.description,
        ].joined(separator: ":")
    }
}

/// A saved world, as the game's `worlds` lists it.
struct World: Identifiable, Equatable {
    let folder: String
    let name: String
    let mode: String
    let lastPlayed: Date
    let version: String
    var id: String { folder }

    init?(_ json: [String: Any]) {
        guard let folder = json["folder"] as? String else { return nil }
        self.folder = folder
        name = json["name"] as? String ?? folder
        mode = json["mode"] as? String ?? "survival"
        lastPlayed = Date(timeIntervalSince1970: (json["last_played"] as? Double ?? 0) / 1000)
        version = json["version"] as? String ?? ""
    }

    /// The picture the game took when the world was last left (`icon.png`), if any.
    var icon: UIImage? {
        UIImage(contentsOfFile: GameFolder.saves.appendingPathComponent(folder).appendingPathComponent("icon.png").path)
    }
}

/// A multiplayer server, as `servers` lists it: its place in servers.dat is its identity.
struct Server: Identifiable, Equatable {
    let index: Int
    let name: String
    let address: String
    let icon: String?
    var id: String { "\(index) \(address)" }
}

/// What a status ping said (`server-ping`).
struct ServerStatus: Equatable {
    var motd = ""
    var online = 0
    var max = 0
    var version = ""
    var ms = 0
    var icon: String?
    var failed: String?
}

/// The game under the launcher: whether it is starting, waiting for files or at its title, and what its
/// title screen would list. Polled while the launcher is up; everything it does goes through `Bridge`.
@MainActor
final class GameLink: ObservableObject {
    enum Phase: Equatable {
        /// No answer yet.
        case unknown
        /// Not started: nothing loads until the player starts the game (Play, a world, a server).
        case waiting
        /// Loading the game files, with how far along.
        case loading(Double)
        /// Nothing to load: the game waits for files (`import_screen.rs`), and says why.
        case needsFiles(String?)
        /// At the title screen: everything works.
        case title
        /// A world is open (the launcher is not normally up then).
        case world
    }

    @Published private(set) var phase: Phase = .unknown
    @Published private(set) var startupStatusError: String?
    @Published private(set) var cleaningUpCache = false
    @Published private var canRetryLoader = false
    private var retryingLoader = false
    @Published private var engineHasGameFiles = false
    @Published private var idleGameFilesProblem: String?
    @Published private(set) var worlds: [World] = []
    @Published private(set) var servers: [Server] = []
    @Published private(set) var statuses: [String: ServerStatus] = [:]
    /// Counts server list reloads, so each one pings the servers again.
    @Published private(set) var serverRound = 0
    @Published private(set) var version = "26.3"
    /// What the Play button says while it waits for the game to be ready to take it somewhere.
    @Published var requestingGameFiles = false
    @Published private(set) var launching: String?
    /// The last thing that went wrong, for an alert.
    @Published var problem: String?

    /// Uncover the game (Present.swift dismisses the launcher).
    var uncover: () -> Void = {}
    /// The launcher can observe published files before their import has replied.
    var gameFilesSetupBusy: () -> Bool = { false }
    /// Installed by HomeView; every Play/Join/Create waits for an acknowledged session.
    var prepareSession: () async throws -> Void = {
        throw AccountError("Account setup is not ready. Try Play again.")
    }
    /// What to ask as soon as the title screen can take it: Play tapped while the game was loading.
    @MainActor
    private final class LaunchAttempt {
        let id = UUID()
        var submitted = false
        var cancelled = false
        var presented = false
        var failure: String?
        var stopProblem: String?
        var preparation: Task<Void, Error>?
        var cleanup: Task<Void, Never>?
    }
    private var currentLaunch: LaunchAttempt?
    private var phaseEpoch = UUID()
    private var appliedRefresh: UUID?
    private var worldListRevision = UUID()
    private var serverListRevision = UUID()
    private var queued: (id: UUID, action: () async -> [String: Any])?
    private let stopAudio: () -> Void
    private let send: (String) async -> [String: Any]

    init(stopAudio: @escaping () -> Void = { GameAudio.shared.stop() },
         send: @escaping (String) async -> [String: Any] = { await Bridge.send($0) }) {
        self.stopAudio = stopAudio
        self.send = send
    }

    /// The engine also supports prepared files beside the executable and at an
    /// explicitly selected root. Check Documents directly for new imports.
    var hasGameFiles: Bool {
        if ready || phase == .world { return true }
        if case .needsFiles = phase { return false }
        if repairingGameFiles { return false }
        return engineHasGameFiles || GameFolder.hasGameFiles
    }

    /// First-time setup or an engine-reported failure to load the installed files.
    var needsGameFiles: Bool {
        if case .needsFiles = phase { return true }
        return phase == .waiting && !cleaningUpCache && !hasGameFiles
    }

    var gameFilesProblem: String? {
        if case let .needsFiles(reason) = phase { return reason }
        if phase == .waiting { return idleGameFilesProblem }
        return nil
    }

    var repairingGameFiles: Bool {
        if phase == .waiting { return idleGameFilesProblem != nil }
        if case .needsFiles = phase { return GameFolder.hasGameFiles }
        return false
    }

    var ready: Bool { phase == .title }
    /// Whether the worlds and servers can be read: they are files, so before the game starts too.
    var canList: Bool { phase == .title || phase == .waiting }

    /// Follow the game while the launcher is up: quickly while it loads, slowly once it is at its title.
    func watch() async {
        while !Task.isCancelled {
            await refresh()
            let settled = ready || phase == .waiting || startupStatusError != nil
            let pause: UInt64 = settled && !cleaningUpCache ? 3_000_000_000 : 400_000_000
            try? await Task.sleep(nanoseconds: pause)
        }
    }

    func refresh() async {
        guard !Task.isCancelled else { return }
        // A pull-to-refresh or setup completion can overlap the watcher. Only
        // the newest read may publish; launch transitions also change this epoch.
        phaseEpoch = UUID()
        let epoch = phaseEpoch
        let state = await self.send("launcher-state")
        guard epoch == phaseEpoch, !Task.isCancelled else { return }
        let was = phase
        let reportedPhase = state.error == nil ? state["phase"] as? String : nil
        switch reportedPhase {
        case "waiting":
            if let attempt = currentLaunch, attempt.cancelled,
               state["stopped_owner"] as? String == attempt.id.uuidString {
                finishCancellation(attempt, stopped: true)
            } else {
                if phase != .waiting { stopAudio() }
                phase = .waiting
            }
        case "loading": phase = .loading(state["progress"] as? Double ?? 0)
        case "needs_files": phase = .needsFiles(state["reason"] as? String)
        case "title": phase = .title
        case "world": phase = .world
        default:
            // Keep the last usable phase, including an open recovery flow. A
            // failed poll can still happen after startup and must be visible.
            let message = state.error ?? "Horizon could not read the game's startup status."
            if startupStatusError != message { startupStatusError = message }
            return
        }
        let retryLoader = state["phase"] as? String == "needs_files" && state["retry_loader"] as? Bool == true
        if canRetryLoader != retryLoader { canRetryLoader = retryLoader }
        if startupStatusError != nil { startupStatusError = nil }
        if phase == .waiting {
            if state["cache_cleanup_in_progress"] as? Bool != true {
                let reportedGameFiles = state["game_files_present"] as? Bool == true
                if engineHasGameFiles != reportedGameFiles { engineHasGameFiles = reportedGameFiles }
                let issue = (state["game_files_problem"] as? String).flatMap { $0.isEmpty ? nil : $0 }
                if idleGameFilesProblem != issue { idleGameFilesProblem = issue }
            }
        }
        // During cleanup the engine can answer this poll ahead of a queued start.
        let collecting = state["cache_cleanup_in_progress"] as? Bool == true
        if cleaningUpCache != collecting { cleaningUpCache = collecting }
        if !collecting { appliedRefresh = epoch }
        if let version = state["version"] as? String {
            self.version = version
        }
        // Keep setup open until any import has replied and a status poll
        // confirms the engine has its files. A marker alone can arrive early.
        if requestingGameFiles, !collecting, !gameFilesSetupBusy(), !needsGameFiles, hasGameFiles {
            requestingGameFiles = false
        }
        if let action = queued {
            if phase == .title {
                queued = nil
                await run(action.action, attempt: action.id)
            } else if case .needsFiles = phase, was != phase {
                requestingGameFiles = true
            }
        }
    }

    /// Read the worlds and servers again.
    func reload() async {
        guard !Task.isCancelled else { return }
        let revision = UUID()
        worldListRevision = revision
        let listed = await self.send("worlds")
        guard !Task.isCancelled, revision == worldListRevision else { return }
        if let list = listed["worlds"] as? [[String: Any]] {
            worlds = list.compactMap(World.init)
        }
        await reloadServers()
    }

    func reloadServers() async {
        guard !Task.isCancelled else { return }
        let revision = UUID()
        serverListRevision = revision
        let listed = await self.send("servers")
        guard !Task.isCancelled, revision == serverListRevision, listed.error == nil,
              let list = listed["servers"] as? [[String: Any]] else { return }
        servers = list.enumerated().compactMap { index, json in
            guard let address = json["address"] as? String else { return nil }
            return Server(index: index, name: json["name"] as? String ?? address, address: address,
                          icon: json["icon"] as? String)
        }
        let activeAddresses = Set(servers.map(\.address))
        statuses = statuses.filter { activeAddresses.contains($0.key) }
        serverRound &+= 1
    }

    var retryInstalledFiles: (() async -> Void)? {
        guard canRetryLoader else { return nil }
        return { await self.retryGameLoader() }
    }

    private func retryGameLoader() async {
        guard canRetryLoader, !retryingLoader, !Task.isCancelled else { return }
        retryingLoader = true
        defer { retryingLoader = false }
        let answer = await send("launcher-retry-loader")
        guard !Task.isCancelled else { return }
        if !answer.acknowledged || answer.error != nil {
            problem = answer.error ?? "Horizon could not retry loading the installed files."
        }
        await refresh()
    }

    // MARK: - Going somewhere

    /// Play: start the game and go to its title screen, or, with a world, into it.
    func play(_ world: World? = nil) async {
        Haptics.launch()
        guard let world else {
            guard let attempt = beginLaunch("Preparing account…") else { return }
            await protectingCancellation(of: attempt) {
                do {
                    try await self.start(attempt)
                    try await self.present(attempt)
                } catch { await self.failed(attempt, error: error) }
            }
            return
        }
        await go("Opening \(world.name)") { await Bridge.world("open", folder: world.folder) }
    }

    func create(_ settings: WorldCreationSettings) async {
        Haptics.launch()
        guard let data = try? JSONEncoder().encode(settings), let json = String(data: data, encoding: .utf8) else {
            problem = "Could not prepare world settings."; return
        }
        await go("Creating \(settings.name)") { await self.send("world-create \(json)") }
    }

    func join(_ address: String) async {
        guard let address = validatedServerAddress(address) else { return }
        Haptics.launch()
        await go("Joining \(address)") { await self.send("connect \(address)") }
    }

    private func validatedServerAddress(_ input: String) -> String? {
        let address = input.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !address.isEmpty, !address.contains(where: \.isWhitespace), !address.utf8.contains(0) else {
            problem = "A server address has no spaces: a host name or IP, with :port if it is not 25565."
            return nil
        }
        return address
    }

    private func beginLaunch(_ label: String) -> LaunchAttempt? {
        guard launching == nil else { return nil }
        // Every entry point (Play, world creation/opening and joining a server)
        // offers setup before starting an engine with missing resources.
        if needsGameFiles {
            requestingGameFiles = true
            return nil
        }
        guard !cleaningUpCache else { return nil }
        let attempt = LaunchAttempt()
        currentLaunch = attempt
        phaseEpoch = UUID()
        launching = label
        return attempt
    }

    private func check(_ attempt: LaunchAttempt) throws {
        try Task.checkCancellation()
        guard currentLaunch === attempt, !attempt.cancelled else { throw CancellationError() }
    }

    /// Task cancellation can arrive while a bridge continuation is suspended.
    /// Cleanup is an independent task, not work inherited by the cancelled caller.
    private func protectingCancellation(of attempt: LaunchAttempt,
                                        operation: @MainActor () async -> Void) async {
        await withTaskCancellationHandler(operation: operation, onCancel: { [weak self] in
            Task { @MainActor in self?.requestCancellation(attempt) }
        })
    }

    private func start(_ attempt: LaunchAttempt) async throws {
        try check(attempt)
        let prepare = prepareSession
        let preparation = Task { @MainActor in
            try Task.checkCancellation()
            try await prepare()
        }
        attempt.preparation = preparation
        defer { attempt.preparation = nil }
        try await withTaskCancellationHandler {
            try await preparation.value
        } onCancel: {
            preparation.cancel()
        }
        try check(attempt)
        // The cached phase can predate Quit/reset. Ask the backend on every
        // intent, and assign ownership before it is allowed to start loading.
        attempt.submitted = true
        phaseEpoch = UUID()
        let refreshAtSubmission = appliedRefresh
        let answer = await self.send("launcher-start \(attempt.id.uuidString)")
        guard answer.acknowledged, answer.error == nil else {
            try check(attempt)
            throw AccountError(answer.error ?? "The game could not start. Try again.")
        }
        try check(attempt)
        // State requests submitted after the start command describe a newer
        // engine state, even when their replies arrive before this one.
        guard appliedRefresh == refreshAtSubmission else { return }
        switch answer["phase"] as? String {
        case "title": phase = .title
        case "world": phase = .world
        case "loading": phase = .loading(0)
        default:
            if phase == .waiting || phase == .unknown { phase = .loading(0) }
        }
    }

    /// Presentation has an explicit backend commit point. Once it commits,
    /// a late cancellation must show that engine instead of leaving it hidden.
    private func present(_ attempt: LaunchAttempt) async throws {
        try check(attempt)
        let answer = await self.send("launcher-commit-start \(attempt.id.uuidString)")
        if answer.acknowledged, answer.error == nil, answer["status"] as? String == "committed" {
            finishPresentation(attempt)
        } else {
            try check(attempt)
            throw AccountError(answer.error ?? "The game could not be shown. Try again.")
        }
    }

    private func finishPresentation(_ attempt: LaunchAttempt) {
        guard currentLaunch === attempt, !attempt.presented else { return }
        attempt.presented = true
        if queued?.id == attempt.id { queued = nil }
        currentLaunch = nil
        phaseEpoch = UUID()
        if phase == .waiting || phase == .unknown { phase = .loading(0) }
        launching = nil
        uncover()
    }

    private func failed(_ attempt: LaunchAttempt, error: Error) async {
        if error is CancellationError || Task.isCancelled || attempt.cancelled {
            requestCancellation(attempt)
        } else if currentLaunch === attempt {
            // The engine may have accepted the command before its reply was
            // lost. Reconcile ownership before letting another Play start.
            if attempt.submitted {
                attempt.failure = error.localizedDescription
                requestCancellation(attempt)
            } else {
                currentLaunch = nil
                launching = nil
                problem = error.localizedDescription
                Haptics.failed()
            }
        }
        if let cleanup = attempt.cleanup { await cleanup.value }
    }

    private func requestCancellation(_ attempt: LaunchAttempt) {
        guard currentLaunch === attempt, !attempt.presented else { return }
        attempt.cancelled = true
        if queued?.id == attempt.id { queued = nil }
        guard attempt.submitted else {
            attempt.preparation?.cancel()
            currentLaunch = nil
            phaseEpoch = UUID()
            launching = nil
            return
        }
        launching = "Stopping game…"
        guard attempt.cleanup == nil else { return }
        attempt.cleanup = Task { @MainActor [weak self] in
            guard let self else { return }
            let answer = await self.send("launcher-cancel-start \(attempt.id.uuidString)")
            guard self.currentLaunch === attempt else { return }
            guard answer.acknowledged, answer.error == nil,
                  let status = answer["status"] as? String,
                  ["stopped", "stale", "committed"].contains(status) else {
                // Do not enable another Play while shutdown is unproven. A
                // second Cancel retries a failed bridge operation.
                attempt.cleanup = nil
                let message = answer.error ?? "The game could not stop. Tap Cancel to retry."
                attempt.stopProblem = message
                self.problem = message
                Haptics.failed()
                return
            }
            if status == "committed" {
                self.clearStopProblem(attempt)
                self.finishPresentation(attempt)
            } else {
                self.finishCancellation(attempt, stopped: status == "stopped")
            }
        }
    }

    private func clearStopProblem(_ attempt: LaunchAttempt) {
        if let message = attempt.stopProblem, problem == message { problem = nil }
    }

    private func finishCancellation(_ attempt: LaunchAttempt, stopped: Bool) {
        clearStopProblem(attempt)
        currentLaunch = nil
        phaseEpoch = UUID()
        launching = nil
        if stopped {
            stopAudio()
            phase = .waiting
        }
        if let failure = attempt.failure {
            problem = failure
            Haptics.failed()
        }
    }

    /// Ask now, or start and wait until the title can accept the action.
    private func go(_ label: String, _ action: @escaping () async -> [String: Any]) async {
        guard let attempt = beginLaunch(label) else { return }
        await protectingCancellation(of: attempt) {
            do {
                try await self.start(attempt)
                if self.ready {
                    await self.run(action, attempt: attempt.id)
                } else {
                    try self.check(attempt)
                    self.queued = (attempt.id, action)
                    if case .needsFiles = self.phase { self.requestingGameFiles = true }
                }
            } catch { await self.failed(attempt, error: error) }
        }
    }

    private func run(_ action: () async -> [String: Any], attempt id: UUID) async {
        guard let attempt = currentLaunch, attempt.id == id else { return }
        await protectingCancellation(of: attempt) {
            do {
                try self.check(attempt)
                let answer = await action()
                try self.check(attempt)
                if let error = answer.error { throw AccountError(error) }
                guard answer.acknowledged else {
                    throw AccountError("Horizon could not confirm that the game accepted the request.")
                }
                try await self.present(attempt)
            } catch { await self.failed(attempt, error: error) }
        }
    }

    /// Before start submission this is immediate. Once submitted, Play remains
    /// disabled until the matching backend cancellation has drained and replied.
    func cancelLaunch() {
        guard let attempt = currentLaunch else { return }
        requestCancellation(attempt)
    }

    // MARK: - Worlds

    func rename(_ world: World, to name: String) async {
        report(await Bridge.world("rename", folder: world.folder, name: name))
        await reload()
    }

    func delete(_ world: World) async {
        report(await Bridge.world("delete", folder: world.folder))
        await reload()
    }

    /// Back a world up into `backups/`, as Edit World's Make Backup does; the size it came to.
    func backup(_ world: World) async -> Int? {
        let answer = await Bridge.world("backup", folder: world.folder)
        report(answer)
        return answer["bytes"] as? Int
    }

    // MARK: - Servers

    func addServer(name: String, address: String) async {
        guard let address = validatedServerAddress(address) else { return }
        let name = name.trimmingCharacters(in: .whitespaces)
        report(await self.send("server-add \(address) \(name.isEmpty ? "Minecraft Server" : name)"))
        await reloadServers()
    }

    func removeServer(_ server: Server) async {
        guard let data = try? JSONSerialization.data(withJSONObject: [
            "index": server.index, "name": server.name, "address": server.address,
        ]) else {
            problem = "Could not prepare the server selection."
            return
        }
        report(await self.send("server-remove \(String(decoding: data, as: UTF8.self))"))
        await reloadServers()
    }

    func ping(_ server: Server) async {
        guard !Task.isCancelled else { return }
        let round = serverRound
        let answer = await self.send("server-ping \(server.address)")
        guard !Task.isCancelled, round == serverRound,
              servers.contains(where: { $0.address == server.address }) else { return }
        var status = ServerStatus()
        if let error = answer.error {
            status.failed = error
        } else if answer.acknowledged, let ms = answer["ms"] as? Int, ms >= 0 {
            status.motd = answer["motd"] as? String ?? ""
            status.online = answer["online"] as? Int ?? 0
            status.max = answer["max"] as? Int ?? 0
            status.version = answer["version"] as? String ?? ""
            status.ms = ms
            status.icon = answer["icon"] as? String
        } else {
            status.failed = "The game did not return a valid server status."
        }
        statuses[server.address] = status
    }

    private func report(_ answer: [String: Any]) {
        if let error = answer.error {
            problem = error
        }
    }
}

/// The taps a launch is felt by.
enum Haptics {
    static func launch() {
        UIImpactFeedbackGenerator(style: .medium).impactOccurred()
    }

    static func tap() {
        UISelectionFeedbackGenerator().selectionChanged()
    }

    static func failed() {
        UINotificationFeedbackGenerator().notificationOccurred(.error)
    }

    static func done() {
        UINotificationFeedbackGenerator().notificationOccurred(.success)
    }
}
