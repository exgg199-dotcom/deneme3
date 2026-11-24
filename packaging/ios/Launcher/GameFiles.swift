import CoreFoundation
import Security
import SwiftUI
import UIKit
import UniformTypeIdentifiers

struct GameFilesOrigin: Codable, Equatable {
    let scheme: String
    let host: String
    let port: Int

    init?(_ url: URL?) {
        guard let url, let scheme = url.scheme?.lowercased(), let host = url.host, !host.isEmpty,
              scheme == "https" || scheme == "http" else { return nil }
        self.scheme = scheme
        self.host = host.lowercased()
        port = url.port ?? (scheme == "https" ? 443 : 80)
    }
}

struct GameFilesCredential: Codable {
    let origin: GameFilesOrigin
    let token: String

    static func token(from data: Data, for origin: GameFilesOrigin?) -> String? {
        // Old Keychain entries contain just a token. Since the saved link was
        // written separately, those entries do not prove which server owns it.
        guard let origin, origin.scheme == "https",
              let credential = try? JSONDecoder().decode(Self.self, from: data),
              credential.origin == origin else { return nil }
        return credential.token
    }
}

struct GameFilesBackgroundTasks {
    let begin: @MainActor (@escaping @MainActor @Sendable () -> Void) -> UIBackgroundTaskIdentifier
    let end: @MainActor (UIBackgroundTaskIdentifier) -> Void

    static let application = Self(
        begin: { UIApplication.shared.beginBackgroundTask(withName: "Horizon game files", expirationHandler: $0) },
        end: { UIApplication.shared.endBackgroundTask($0) }
    )
}

/// Private download staging. Each static instance cleans only its own files,
/// once per process, before any attempt can create a new archive.
struct ArchiveStaging: Sendable {
    static let gameFiles = Self(prefix: "horizon-download-")
    static let packs = Self(prefix: "horizon-pack-download-")

    private let prefix: String
    private let directory: URL

    private init(prefix: String) {
        self.prefix = prefix
        directory = FileManager.default.temporaryDirectory
        Self.removeAbandoned(in: directory, prefix: prefix)
    }

    func nextFile() -> URL {
        directory.appendingPathComponent("\(prefix)\(UUID().uuidString).zip")
    }

    static func removeAbandoned(in directory: URL, prefix: String) {
        let manager = FileManager.default
        let keys: Set<URLResourceKey> = [.isRegularFileKey, .isSymbolicLinkKey]
        guard let entries = try? manager.contentsOfDirectory(
            at: directory, includingPropertiesForKeys: Array(keys)
        ) else { return }
        for file in entries {
            let name = file.deletingPathExtension().lastPathComponent
            guard file.pathExtension == "zip", name.hasPrefix(prefix),
                  UUID(uuidString: String(name.dropFirst(prefix.count))) != nil,
                  let values = try? file.resourceValues(forKeys: keys),
                  values.isRegularFile == true, values.isSymbolicLink == false else { continue }
            try? manager.removeItem(at: file)
        }
    }
}

/// Prepared game files downloaded automatically on first launch, or imported from a custom link.
/// Both routes use the game's checked importer. Raw Java assets are prepared on a computer.
@MainActor
final class GameFilesDownload: NSObject, ObservableObject {
    enum State: Equatable {
        case idle
        case findingSource
        case downloading(received: Int64, total: Int64)
        case preparingArchive
        case cancellingArchive
        case installing
        case done(String)
        case failed(String)
    }

    @Published private(set) var state: State = .idle
    @Published var link: String {
        didSet {
            if Self.origin(of: oldValue) != Self.origin(of: link) {
                token = ""
            }
        }
    }
    @Published var token: String

    private static func origin(of link: String) -> GameFilesOrigin? {
        GameFilesOrigin(URL(string: link.trimmingCharacters(in: .whitespacesAndNewlines)))
    }

    typealias ImportArchive = @MainActor (String) async -> [String: Any]
    private let importArchive: ImportArchive
    private let automaticSource: @MainActor () async -> [String: Any]
    private let diagnostic: @MainActor (String) -> Void
    private let backgroundTasks: GameFilesBackgroundTasks
    private var startedAt = ProcessInfo.processInfo.systemUptime
    private var lastProgressAt = 0.0
    private var receivedBytes: Int64 = 0
    private var expectedBytes: Int64 = 0

    private func trace(_ event: String) {
        let elapsed = Int(max(0, ProcessInfo.processInfo.systemUptime - startedAt) * 1000)
        diagnostic("Game files [\(attempt.uuidString.prefix(8))] \(event) elapsed_ms=\(elapsed)")
    }

    private func traceStorage() {
        let free = (try? FileManager.default.attributesOfFileSystem(forPath: GameFolder.documents.path)[.systemFreeSize] as? NSNumber)?.int64Value
        trace("storage free_bytes=\(free ?? -1)")
    }

    /// Never log NSError.userInfo/descriptions: these can contain URLs and credentials.
    nonisolated static func errorFacts(_ error: Error) -> String {
        let e = error as NSError
        let domain = [NSURLErrorDomain, NSPOSIXErrorDomain, NSCocoaErrorDomain].contains(e.domain) ? e.domain : "other"
        let kind: String
        switch (e.domain, e.code) {
        case (NSURLErrorDomain, NSURLErrorCannotFindHost): kind = "dns"
        case (NSURLErrorDomain, NSURLErrorCannotConnectToHost): kind = "connection"
        case (NSURLErrorDomain, NSURLErrorTimedOut): kind = "timeout"
        case (NSURLErrorDomain, NSURLErrorNotConnectedToInternet): kind = "offline"
        case (NSURLErrorDomain, NSURLErrorNetworkConnectionLost): kind = "connection_lost"
        case (NSURLErrorDomain, NSURLErrorCancelled): kind = "cancelled"
        case (NSURLErrorDomain, -1206 ... -1200): kind = "tls"
        case (NSCocoaErrorDomain, NSFileWriteOutOfSpaceError): kind = "storage_full"
        case (NSCocoaErrorDomain, NSFileReadTooLargeError): kind = "archive_too_large"
        default: kind = "other"
        }
        var facts = "kind=\(kind) domain=\(domain) code=\(e.code)"
        if let underlying = e.userInfo[NSUnderlyingErrorKey] as? NSError {
            let safe = [NSURLErrorDomain, NSPOSIXErrorDomain, NSCocoaErrorDomain].contains(underlying.domain) ? underlying.domain : "other"
            facts += " underlying_domain=\(safe) underlying_code=\(underlying.code)"
        }
        return facts
    }
    private var automaticAttempted = false
    // Same packed-ZIP limit as horizon_client::import::MAX_PACKED. Enforce
    // it during transfer, before an oversized archive fills the sandbox.
    private nonisolated static let maximumArchiveBytes: Int64 = 2 << 30
    private nonisolated static let archiveLimitMessage = "The archive exceeds the 2 GiB game-file limit."
    private var automaticDownload = false
    private var resourcesOnly = false
    private var rejectedMarker: String?
    private var sourceLookup: Task<[String: Any], Never>?
    private var stagingCopy: Task<Void, Error>?
    private var attempt = UUID()
    private var session: URLSession?
    private var retryRequest: URLRequest?

    init(importArchive: @escaping ImportArchive = { await Bridge.send($0) },
         automaticSource: @escaping @MainActor () async -> [String: Any] = { await Bridge.send("game-files-source") },
         diagnostic: @escaping @MainActor (String) -> Void = { Diagnostics.record($0) },
         backgroundTasks: GameFilesBackgroundTasks = .application) {
        let savedLink = UserDefaults.standard.string(forKey: "gameFilesLink") ?? ""
        self.link = savedLink
        self.token = Keychain.read(for: Self.origin(of: savedLink)) ?? ""
        self.importArchive = importArchive
        self.automaticSource = automaticSource
        self.diagnostic = diagnostic
        self.backgroundTasks = backgroundTasks
        super.init()
        _ = ArchiveStaging.gameFiles
    }
    private var background: UIBackgroundTaskIdentifier = .invalid

    var busy: Bool {
        canCancel || state == .cancellingArchive || state == .installing
    }

    var canCancel: Bool {
        switch state {
        case .findingSource, .downloading, .preparingArchive: return true
        default: return false
        }
    }

    /// Only automatic setup failures belong in the Play panel. Custom-link and
    /// local-file errors remain in their own setup UI.
    var automaticFailure: String? {
        guard automaticDownload, case let .failed(message) = state else { return nil }
        return message
    }

    func start(gameFilesOnly: Bool = false) {
        guard !busy else { return }
        attempt = UUID()
        startedAt = ProcessInfo.processInfo.systemUptime
        automaticDownload = false
        resourcesOnly = gameFilesOnly
        trace("custom_start")
        let text = link.trimmingCharacters(in: .whitespacesAndNewlines)
        guard let url = URL(string: text), let origin = GameFilesOrigin(url)
        else {
            trace("custom_source_rejected invalid_url")
            state = .failed("That is not a web link. It starts with https:// (or http:// on your own network).")
            return
        }
        let token = token.trimmingCharacters(in: .whitespacesAndNewlines)
        guard token.isEmpty || origin.scheme == "https" else {
            trace("custom_source_rejected insecure_credential")
            state = .failed("Use an HTTPS link to download with an access token.")
            return
        }
        do {
            try Keychain.write(token.isEmpty ? nil : GameFilesCredential(origin: origin, token: token))
        } catch {
            trace("credential_storage_failed \(Self.errorFacts(error))")
            state = .failed(error.localizedDescription)
            return
        }
        UserDefaults.standard.set(text, forKey: "gameFilesLink")
        begin(url: url, token: token.isEmpty ? nil : token)
    }

    /// First-run setup stays in the launcher; it starts no game or renderer.
    func startAutomaticallyIfNeeded() async {
        guard !GameFolder.hasGameFiles, !automaticAttempted, state == .idle, !busy else { return }
        await startAutomatic()
    }

    func startAutomatic(replacingRejectedFiles: Bool = false) async {
        guard !busy, !Task.isCancelled else { return }
        automaticAttempted = true
        automaticDownload = true
        resourcesOnly = true
        rejectedMarker = replacingRejectedFiles ? GameFolder.resourceMarker(at: GameFolder.resources) : nil
        attempt = UUID()
        startedAt = ProcessInfo.processInfo.systemUptime
        let generation = attempt
        receivedBytes = 0
        expectedBytes = 0
        state = .findingSource
        trace("source_lookup_start")
        let source = automaticSource
        let lookup = Task { await source() }
        sourceLookup = lookup
        let answer = await withTaskCancellationHandler {
            await lookup.value
        } onCancel: {
            lookup.cancel()
        }
        guard generation == attempt else { return }
        sourceLookup = nil
        trace("source_lookup_result has_url=\(answer["url"] is String) has_error=\(answer.error != nil)")
        guard !Task.isCancelled else {
            // The view stopped observing setup. Its next appearance may try again.
            automaticAttempted = false
            trace("source_lookup_cancelled")
            state = .idle
            return
        }
        // A cable/manual import can finish while the source request is pending.
        guard !keepInstalledGameFiles() else {
            trace("already_installed")
            state = .done("Game files are already installed.")
            return
        }
        // A failed lookup may still include a stale URL. Require success before transferring.
        guard answer.error == nil, let text = answer["url"] as? String,
              let url = URL(string: text), GameFilesOrigin(url) != nil else {
            trace("source_lookup_failed detail=\(Diagnostics.redact(answer.error ?? "missing_or_invalid_url"))")
            state = .failed(answer.error ?? "The automatic download is unavailable. Retry or use a download link.")
            return
        }
        // A custom link's token belongs only to that link, never the public source.
        begin(url: url, token: nil)
    }

    private func keepInstalledGameFiles() -> Bool {
        guard let current = GameFolder.resourceMarker(at: GameFolder.resources) else { return false }
        // Keep a newer installation, even when this attempt was asked to repair the old one.
        return current != rejectedMarker
    }

    /// Enforce the archive limit on the bytes copied, even if the provider's
    /// size metadata is missing, stale, or the file changes during hydration.
    nonisolated static func stageArchive(from source: URL, to destination: URL, maximumBytes: Int64) throws {
        try Task.checkCancellation()
        let input = try FileHandle(forReadingFrom: source)
        defer { try? input.close() }
        try Data().write(to: destination, options: .withoutOverwriting)
        let output = try FileHandle(forWritingTo: destination)
        defer { try? output.close() }

        var remaining = maximumBytes
        while true {
            try Task.checkCancellation()
            // Read at most one byte past the limit to distinguish EOF from
            // an oversized file without writing anything beyond the limit.
            let readCount = Int(min((1 << 20) - 1, remaining)) + 1
            guard let bytes = try input.read(upToCount: readCount), !bytes.isEmpty else { return }
            guard Int64(bytes.count) <= remaining else { throw CocoaError(.fileReadTooLarge) }
            try output.write(contentsOf: bytes)
            remaining -= Int64(bytes.count)
        }
    }

    /// Import through the same resource-only validator as automatic setup.
    func importPreparedFile(_ url: URL) async {
        guard !busy, !Task.isCancelled else { return }
        automaticDownload = false
        attempt = UUID()
        startedAt = ProcessInfo.processInfo.systemUptime
        trace("file_import_start")
        traceStorage()
        let generation = attempt
        defer {
            if generation == attempt {
                stagingCopy = nil
                if state == .preparingArchive || state == .cancellingArchive {
                    trace("file_import_cancelled")
                    state = .idle
                }
            }
        }
        let scoped = url.startAccessingSecurityScopedResource()
        defer { if scoped { url.stopAccessingSecurityScopedResource() } }
        let local = ArchiveStaging.gameFiles.nextFile()
        defer { try? FileManager.default.removeItem(at: local) }

        // A provider may still be fetching the ZIP. Keep the preparation
        // cancellable and hold the security scope until the worker exits.
        state = .preparingArchive
        let copy = Task.detached(priority: .utility) {
            try Task.checkCancellation()
            var source = url
            source.removeAllCachedResourceValues()
            let values = try source.resourceValues(forKeys: [.fileSizeKey, .isRegularFileKey])
            guard values.isRegularFile == true else { throw DownloadError("Choose a regular ZIP file.") }
            if let size = values.fileSize, Int64(size) > Self.maximumArchiveBytes {
                throw CocoaError(.fileReadTooLarge)
            }
            try Self.stageArchive(from: source, to: local, maximumBytes: Self.maximumArchiveBytes)
        }
        stagingCopy = copy
        trace("file_copy_start")
        do {
            try await withTaskCancellationHandler {
                try await copy.value
            } onCancel: {
                copy.cancel()
            }
            guard generation == attempt, !copy.isCancelled, !Task.isCancelled else { return }
            stagingCopy = nil
            // The native importer owns the staged file from here to its reply.
            state = .installing
            let answer = await importArchive("import-game-files \(local.path)")
            guard generation == attempt else { return }
            completeImport(answer, resourcesOnly: true, event: "file_import")
        } catch {
            guard generation == attempt, !copy.isCancelled, !Task.isCancelled else { return }
            trace("file_import_failed \(Self.errorFacts(error))")
            state = .failed(Self.fileImportMessage(error))
            Haptics.failed()
        }
    }

    nonisolated static func fileImportMessage(_ error: Error) -> String {
        guard let error = error as? CocoaError, error.code == .fileReadTooLarge else {
            return error.localizedDescription
        }
        return archiveLimitMessage
    }

    private func begin(url: URL, token: String?) {
        // Only the shipped public endpoint gets a name. Custom paths/queries stay private.
        let endpoint = url.host?.lowercased() == "cdn.gayfeet.lol" ? "default-cdn" : "custom"
        trace("transfer_start endpoint=\(endpoint) secure=\(url.scheme?.lowercased() == "https")")
        traceStorage()
        var request = URLRequest(url: url)
        if let token { request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization") }
        // A prepared tree is ~600 MB: ask for the time to finish if the player looks away.
        let generation = attempt
        background = backgroundTasks.begin { [weak self] in
            guard let self, self.attempt == generation, self.background != .invalid else { return }
            // UIKit gives this callback a synchronous chance to end the task before suspension.
            self.endBackground()
            self.trace("background_time_expired")
        }
        retryRequest = request
        startTransfer(request)
    }

    private func startTransfer(_ request: URLRequest) {
        receivedBytes = 0
        expectedBytes = 0
        lastProgressAt = ProcessInfo.processInfo.systemUptime
        let session = URLSession(configuration: .default, delegate: self, delegateQueue: nil)
        session.sessionDescription = String(attempt.uuidString.prefix(8))
        self.session = session
        state = .downloading(received: 0, total: 0)
        session.downloadTask(with: request).resume()
    }

    func cancel() {
        // The importer owns the archive once installation begins. Releasing
        // the busy state sooner would let a second operation overlap it.
        guard canCancel else { return }
        if state == .preparingArchive {
            stagingCopy?.cancel()
            state = .cancellingArchive
            return
        }
        trace("cancel_requested received=\(receivedBytes) expected=\(expectedBytes)")
        attempt = UUID()
        sourceLookup?.cancel()
        sourceLookup = nil
        session?.invalidateAndCancel()
        session = nil
        retryRequest = nil
        state = .idle
        endBackground()
    }

    private func endBackground() {
        let identifier = background
        guard identifier != .invalid else { return }
        background = .invalid
        backgroundTasks.end(identifier)
    }

    private func finished(_ outcome: Result<URL, Error>, from source: URLSession) async {
        guard session === source else {
            if case let .success(file) = outcome { try? FileManager.default.removeItem(at: file) }
            return
        }
        let generation = attempt
        session?.finishTasksAndInvalidate()
        session = nil
        let retry = retryRequest
        retryRequest = nil
        switch outcome {
        case let .failure(error):
            trace("transfer_failed \(Self.errorFacts(error)) received=\(receivedBytes) expected=\(expectedBytes)")
            if automaticDownload && keepInstalledGameFiles() {
                state = .done("Game files are already installed.")
                break
            }
            let networkError = error as NSError
            if let retry, networkError.domain == NSURLErrorDomain,
               networkError.code == NSURLErrorNetworkConnectionLost {
                // Restart the original GET once. A fresh session keeps late
                // progress from the failed task out of the new transfer.
                trace("transfer_restarted reason=connection_lost")
                startTransfer(retry)
                return
            }
            state = .failed(error.localizedDescription)
            Haptics.failed()
        case let .success(file):
            trace("transfer_complete received=\(receivedBytes) expected=\(expectedBytes)")
            defer { try? FileManager.default.removeItem(at: file) }
            // A manual/cable import can also finish after the HTTP transfer began.
            guard !automaticDownload || !keepInstalledGameFiles() else {
                state = .done("Game files are already installed.")
                break
            }
            state = .installing
            trace("import_start")
            traceStorage()
            let command = resourcesOnly ? "import-game-files" : "import"
            let answer: [String: Any]
            if automaticDownload {
                // The native installer repeats the check while holding the home lock.
                do {
                    let payload = ["path": file.path, "resource_marker": rejectedMarker ?? "missing"]
                    let json = String(decoding: try JSONEncoder().encode(payload), as: UTF8.self)
                    answer = await importArchive("\(command) \(json)")
                } catch {
                    answer = ["error": "Horizon could not start the game-file installation."]
                }
            } else {
                answer = await importArchive("\(command) \(file.path)")
            }
            guard generation == attempt else { return }
            completeImport(answer, resourcesOnly: resourcesOnly, event: "import")
        }
        endBackground()
    }

    private func completeImport(_ answer: [String: Any], resourcesOnly: Bool, event: String) {
        do {
            let message = try Self.importMessage(answer, resourcesOnly: resourcesOnly)
            trace("\(event)_complete files_ready=\(GameFolder.hasGameFiles)")
            state = .done(message)
            Haptics.done()
        } catch {
            let message = error.localizedDescription
            trace("\(event)_failed detail=\(Diagnostics.redact(message))")
            state = .failed(message)
            Haptics.failed()
        }
    }

    private static func importMessage(_ answer: [String: Any], resourcesOnly: Bool) throws -> String {
        if let error = answer.error { throw DownloadError(error) }
        guard let ok = answer["ok"] as? NSNumber, CFGetTypeID(ok) == CFBooleanGetTypeID(), ok.boolValue,
              let kind = answer["kind"] as? String else {
            throw DownloadError("Horizon could not confirm that the archive was installed.")
        }
        if kind == "resources" || (resourcesOnly && kind == "resources_already_installed") {
            guard GameFolder.hasGameFiles else {
                throw DownloadError("The installed game files are missing. Try installing them again.")
            }
            if kind == "resources_already_installed" { return "Game files are already installed." }
            return "Game files installed. If the game was already running, close Horizon and open it again to use them."
        }
        if resourcesOnly {
            throw DownloadError("This archive did not install game files.")
        }
        guard let name = answer["name"] as? String, !name.isEmpty else {
            throw DownloadError("Horizon could not confirm that the archive was installed.")
        }
        switch kind {
        case "world": return "World \(name) added."
        case "resource_pack": return "Resource pack \(name) added."
        default: throw DownloadError("Horizon could not confirm that the archive was installed.")
        }
    }
}

extension GameFilesDownload: URLSessionDownloadDelegate {
    /// Keep HTTPS throughout the download and share credentials only within the same origin.
    nonisolated static func redirectRequest(_ request: URLRequest, from source: URL?) -> URLRequest? {
        guard let origin = GameFilesOrigin(source), let target = GameFilesOrigin(request.url),
              origin.scheme != "https" || target.scheme == "https" else { return nil }
        var request = request
        if origin.scheme != "https" || origin != target {
            request.setValue(nil, forHTTPHeaderField: "Authorization")
        }
        return request
    }

    nonisolated func urlSession(_ session: URLSession, task: URLSessionTask,
                                willPerformHTTPRedirection response: HTTPURLResponse,
                                newRequest request: URLRequest,
                                completionHandler: @escaping (URLRequest?) -> Void) {
        completionHandler(Self.redirectRequest(request, from: response.url))
    }

    nonisolated func urlSession(_ session: URLSession, task: URLSessionTask, didFinishCollecting metrics: URLSessionTaskMetrics) {
        let id = session.sessionDescription ?? "unknown"
        func ms(_ start: Date?, _ end: Date?) -> Int {
            guard let start, let end else { return -1 }
            return Int(max(0, end.timeIntervalSince(start)) * 1000)
        }
        let last = metrics.transactionMetrics.last
        let status = (last?.response as? HTTPURLResponse)?.statusCode ?? 0
        let facts = "Game files [\(id)] network_metrics redirects=\(metrics.redirectCount) transactions=\(metrics.transactionMetrics.count) http_status=\(status) dns_ms=\(ms(last?.domainLookupStartDate,last?.domainLookupEndDate)) connect_ms=\(ms(last?.connectStartDate,last?.connectEndDate)) tls_ms=\(ms(last?.secureConnectionStartDate,last?.secureConnectionEndDate)) response_ms=\(ms(last?.responseStartDate,last?.responseEndDate))"
        // Metrics may arrive after the terminal callback. Keep their original attempt ID.
        Task { @MainActor in self.diagnostic(facts) }
    }

    nonisolated func urlSession(_ session: URLSession, downloadTask: URLSessionDownloadTask, didWriteData _: Int64,
                                totalBytesWritten: Int64, totalBytesExpectedToWrite: Int64) {
        let oversized = totalBytesWritten > Self.maximumArchiveBytes || totalBytesExpectedToWrite > Self.maximumArchiveBytes
        if oversized {
            downloadTask.taskDescription = Self.archiveLimitMessage
            downloadTask.cancel()
        }
        Task { @MainActor in
            guard self.session === session else { return }
            self.receivedBytes = totalBytesWritten
            self.expectedBytes = max(totalBytesExpectedToWrite, 0)
            let now = ProcessInfo.processInfo.systemUptime
            if oversized || now - self.lastProgressAt >= 15 {
                self.lastProgressAt = now
                self.trace("transfer_progress received=\(totalBytesWritten) expected=\(max(totalBytesExpectedToWrite, 0)) oversized=\(oversized)")
            }
            guard !oversized else { return }
            self.state = .downloading(received: totalBytesWritten, total: max(totalBytesExpectedToWrite, 0))
        }
    }

    nonisolated func urlSession(_ session: URLSession, downloadTask: URLSessionDownloadTask,
                                didFinishDownloadingTo location: URL) {
        // Move the temporary file before this callback returns.
        let outcome: Result<URL, Error>
        let response = downloadTask.response as? HTTPURLResponse
        let status = response?.statusCode ?? 0
        if downloadTask.taskDescription == Self.archiveLimitMessage
            || downloadTask.countOfBytesReceived > Self.maximumArchiveBytes || downloadTask.countOfBytesExpectedToReceive > Self.maximumArchiveBytes {
            outcome = .failure(DownloadError(Self.archiveLimitMessage))
        } else if response == nil {
            outcome = .failure(DownloadError("The download did not receive an HTTP response."))
        } else if !(200..<300).contains(status) {
            let reason = status == 401 || status == 403
                ? "The download server refused the request (\(status))."
                : "The server answered \(status) \(HTTPURLResponse.localizedString(forStatusCode: status))."
            outcome = .failure(DownloadError(reason))
        } else if status == 206 || response?.value(forHTTPHeaderField: "Content-Range") != nil {
            // This downloader always requests the whole archive.
            outcome = .failure(DownloadError("The server returned only part of the archive. Retry or use another link."))
        } else if status == 204 || status == 205 {
            outcome = .failure(DownloadError("The download server did not send an archive."))
        } else {
            do {
                // Transfer counters describe the response. Validate the actual
                // file before asking the engine to open it as an archive.
                let bytes = try location.resourceValues(forKeys: [.fileSizeKey]).fileSize
                if bytes == 0 {
                    throw DownloadError("The download server did not send an archive.")
                }
                if let bytes, Int64(bytes) > Self.maximumArchiveBytes {
                    throw DownloadError(Self.archiveLimitMessage)
                }
                let destination = ArchiveStaging.gameFiles.nextFile()
                try FileManager.default.createDirectory(at: destination.deletingLastPathComponent(), withIntermediateDirectories: true)
                try? FileManager.default.removeItem(at: destination)
                try FileManager.default.moveItem(at: location, to: destination)
                outcome = .success(destination)
            } catch {
                outcome = .failure(error)
            }
        }
        let received = downloadTask.countOfBytesReceived
        let expected = max(downloadTask.countOfBytesExpectedToReceive, 0)
        Task { @MainActor in
            guard self.session === session else {
                if case let .success(file) = outcome { try? FileManager.default.removeItem(at: file) }
                return
            }
            self.receivedBytes = received
            self.expectedBytes = expected
            self.trace("http_response status=\(status) received=\(received) expected=\(expected)")
            await self.finished(outcome, from: session)
        }
    }

    nonisolated func urlSession(_ session: URLSession, task: URLSessionTask, didCompleteWithError error: Error?) {
        guard let error else { return }
        let failure: Error = task.taskDescription == Self.archiveLimitMessage
            || task.countOfBytesReceived > Self.maximumArchiveBytes || task.countOfBytesExpectedToReceive > Self.maximumArchiveBytes
            ? DownloadError(Self.archiveLimitMessage) : error
        Task { @MainActor in
            guard self.session === session else { return }
            self.receivedBytes = task.countOfBytesReceived
            self.expectedBytes = max(task.countOfBytesExpectedToReceive, 0)
            await self.finished(.failure(failure), from: session)
        }
    }
}

private struct DownloadError: LocalizedError {
    let errorDescription: String?
    init(_ reason: String) { errorDescription = reason }
}

/// The access token, kept in the keychain rather than in preferences.
private enum Keychain {
    private static let query: [String: Any] = [
        kSecClass as String: kSecClassGenericPassword,
        kSecAttrService as String: "dev.horizon.launcher",
        kSecAttrAccount as String: "game-files-token",
    ]

    static func read(for origin: GameFilesOrigin?) -> String? {
        var item: CFTypeRef?
        var search = query
        search[kSecReturnData as String] = true
        guard SecItemCopyMatching(search as CFDictionary, &item) == errSecSuccess, let data = item as? Data else {
            return nil
        }
        return GameFilesCredential.token(from: data, for: origin)
    }

    static func write(_ credential: GameFilesCredential?) throws {
        guard let credential else {
            let status = SecItemDelete(query as CFDictionary)
            guard status == errSecSuccess || status == errSecItemNotFound else { throw storageError() }
            return
        }
        // Never remove a working credential before its replacement is saved.
        let values: [String: Any] = [kSecValueData as String: try JSONEncoder().encode(credential),
            kSecAttrAccessible as String: kSecAttrAccessibleAfterFirstUnlock]
        var status = SecItemUpdate(query as CFDictionary, values as CFDictionary)
        if status == errSecItemNotFound {
            var item = query
            values.forEach { item[$0.key] = $0.value }
            status = SecItemAdd(item as CFDictionary, nil)
        }
        guard status == errSecSuccess else { throw storageError() }
    }

    private static func storageError() -> DownloadError {
        DownloadError("The download credential could not be updated securely. Unlock this device and try again.")
    }
}

// MARK: - The card

/// Get the game files: the link, the token, the progress and what happened.
struct GameFilesCard: View {
    @ObservedObject var download: GameFilesDownload
    /// Whether the player needs to install or repair the game files.
    var missing: Bool
    var repairing = false
    var setupHelp: (() -> Void)? = nil
    var retryLoader: (() async -> Void)? = nil
    var offersFileImport = false

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(spacing: 8) {
                Image(systemName: missing ? "exclamationmark.triangle.fill" : "arrow.down.circle.fill")
                    .foregroundStyle(missing ? Theme.warn : Theme.accentText)
                Text(missing ? "Game files needed" : "Download game files")
                    .font(Theme.pixel(18)).foregroundStyle(Theme.text)
            }
            if missing, retryLoader == nil {
                Text("Keep this screen open until game-file setup finishes.")
                    .font(Theme.sans(12)).foregroundStyle(Theme.textMuted)
            }
            progress
            if missing, let setupHelp {
                Button("Other ways to get game files", action: setupHelp)
                    .accessibilityIdentifier("launcher-game-files-help")
            }

            if download.busy {
                SecondaryButton(title: "Cancel", action: download.cancel)
                    .disabled(!download.canCancel)
            } else {
                if let retryLoader {
                    PrimaryButton(title: "Retry loading game files", icon: "arrow.clockwise") {
                        Task { await retryLoader() }
                    }
                } else if missing {
                    PrimaryButton(title: repairing ? "Reinstall game files" : "Download game files", icon: "arrow.down") {
                        Task { await download.startAutomatic(replacingRejectedFiles: repairing) }
                    }
                    .accessibilityIdentifier("launcher-download-game-files")
                }
            }
            if offersFileImport {
                PreparedGameFilesPicker(download: download)
            }
            DisclosureGroup(missing ? "Use a download link instead" : "Download from a link") {
                Text("""
                Horizon plays with the files your own Minecraft: Java Edition makes. Prepare them on a computer \
                (horizon-prepare, then pack them as a zip), put the zip somewhere this phone can reach, such as your \
                computer on this Wi-Fi or a server of your own, and paste its link.
                """)
                .font(Theme.sans(12)).foregroundStyle(Theme.textMuted)
                .fixedSize(horizontal: false, vertical: true)
                if !missing {
                    Text("World and resource pack ZIPs can also be imported from a link.")
                        .font(Theme.sans(12)).foregroundStyle(Theme.textMuted)
                }

                Field(title: "LINK", placeholder: "https://example.com/horizon-26.3.zip", text: $download.link,
                      secure: false)
                Field(title: "ACCESS TOKEN (OPTIONAL)", placeholder: "Sent as a Bearer token", text: $download.token,
                      secure: true)

                PrimaryButton(title: "Download from link", icon: "arrow.down") {
                    download.start(gameFilesOnly: missing)
                }
                .disabled(download.busy || download.link.trimmingCharacters(in: .whitespaces).isEmpty)
            }
            Text("You can also send the zip from the game's own screen over Wi-Fi, or drop it into Horizon's folder in the Files app.")
                .font(Theme.sans(11)).foregroundStyle(Theme.textFaint)
                .fixedSize(horizontal: false, vertical: true)
        }
        .padding(16)
        .background(Theme.surface, in: RoundedRectangle(cornerRadius: 12))
        .overlay(RoundedRectangle(cornerRadius: 12).strokeBorder(missing ? Theme.noticeBorder : Theme.border))
    }

    @ViewBuilder private var progress: some View {
        switch download.state {
        case .idle:
            EmptyView()
        case .findingSource:
            importProgress("Finding game files…")
        case let .downloading(received, total):
            VStack(alignment: .leading, spacing: 6) {
                if total > 0 {
                    ProgressView(value: Double(received), total: Double(total)).tint(Theme.accent)
                } else {
                    ProgressView().tint(Theme.accent)
                }
                Text(total > 0
                    ? "\(Self.size(received)) of \(Self.size(total))"
                    : "\(Self.size(received)) downloaded")
                    .font(Theme.mono(10)).foregroundStyle(Theme.textMuted)
            }
        case .preparingArchive:
            importProgress("Preparing archive…")
        case .cancellingArchive:
            importProgress("Canceling archive preparation…")
        case .installing:
            importProgress("Unpacking…")
        case let .done(message):
            Label(message, systemImage: "checkmark.circle.fill")
                .font(Theme.sans(12)).foregroundStyle(Theme.ready)
        case let .failed(message):
            Label(message, systemImage: "xmark.octagon.fill")
                .font(Theme.sans(12)).foregroundStyle(Theme.danger)
        }
    }

    private func importProgress(_ title: String) -> some View {
        HStack(spacing: 8) {
            ProgressView().tint(Theme.accent)
            Text(title).font(Theme.mono(10)).foregroundStyle(Theme.textMuted)
        }
    }

    private static func size(_ bytes: Int64) -> String {
        ByteCountFormatter.string(fromByteCount: bytes, countStyle: .file)
    }
}


private struct PreparedGameFilesPicker: View {
    @ObservedObject var download: GameFilesDownload
    @State private var choosingFile = false
    @State private var fileIssue: String?

    var body: some View {
        Button("Choose prepared ZIP") { choosingFile = true }
            .disabled(download.busy)
            .accessibilityIdentifier("launcher-import-prepared-files")
            .fileImporter(isPresented: $choosingFile, allowedContentTypes: [.zip]) { result in
                switch result {
                case let .success(url): Task { await download.importPreparedFile(url) }
                case let .failure(error): fileIssue = error.localizedDescription
                }
            }
            .alert("Couldn’t open game files", isPresented: Binding(
                get: { fileIssue != nil }, set: { if !$0 { fileIssue = nil } }
            )) { Button("OK", role: .cancel) {} } message: { Text(fileIssue ?? "") }
    }
}

/// Recovery appears on an explicit play/setup request, never over background setup.
struct GameFilesRecovery: View {
    @ObservedObject var download: GameFilesDownload
    let version: String
    let signedIn: Bool
    var repairing = false
    var engineProblem: String? = nil
    var retryLoader: (() async -> Void)? = nil
    var configureManually: (() -> Void)? = nil
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    let reason = engineProblem.flatMap { $0.isEmpty ? nil : $0 }
                    if let reason {
                        Text("Horizon could not load the game files.").font(.headline)
                        Text(reason).foregroundStyle(.secondary).textSelection(.enabled)
                            .accessibilityIdentifier("launcher-game-files-engine-problem")
                    }
                    if case let .failed(message) = download.state {
                        Text(download.automaticFailure != nil ? "Automatic file download failed." : "Game-file setup failed.")
                            .font(.headline)
                        Text(message).foregroundStyle(.secondary).textSelection(.enabled)
                        Text("You can retry, or go to Settings → Game files to configure your own files manually.")
                    } else if download.busy {
                        Text("Game files are being set up. You can play when installation finishes.")
                    } else if reason == nil {
                        Text("Horizon needs Minecraft game files before you can play, open a world or join a server.")
                    }
                    Text("You can retry automatic setup below, or prepare and transfer the files from a computer.")
                }
                Section("Download from Mojang with Microsoft") {
                    Text(signedIn
                         ? "Your Microsoft account is signed in here. To prepare game files, also sign in to the Horizon desktop launcher with an account that owns Minecraft: Java Edition."
                         : "On a computer, sign in to the Horizon desktop launcher with a Microsoft account that owns Minecraft: Java Edition.")
                    Text("Choose Download and prepare for Minecraft \(version). The desktop launcher downloads from Mojang and prepares the files for Horizon.")
                    Text("Signing in on this device alone does not download or prepare these files yet. Transfer the prepared files using the steps below.")
                        .foregroundStyle(.secondary)
                    Link("Get the Horizon desktop launcher", destination: URL(string: "https://github.com/horizon-rust/horizon/releases")!)
                }
                Section("Prepare or transfer your own files") {
                    if let configureManually {
                        Button("Open Settings → Game files", action: configureManually)
                            .accessibilityIdentifier("launcher-game-files-open-settings")
                    }
                    Text("If Minecraft \(version) is already installed on your computer, use the desktop launcher’s From my install option to prepare it instead.")
                    Text("Export the prepared folder with the desktop game executable:")
                    Text("horizon-client --pack-resources <prepared-folder> --out Horizon-\(version).zip")
                        .font(.footnote.monospaced()).textSelection(.enabled)
                    Text("Replace <prepared-folder> with the folder created by preparation. A Minecraft JAR, world ZIP or resource pack alone cannot replace these files.")
                    Text("Copy the prepared ZIP into Horizon’s Documents folder using Finder or Apple Devices file sharing. You can also put it at a link this device can reach and use Download from link below.")
                    PreparedGameFilesPicker(download: download)
                    Text("Choose the ZIP from Files after transferring it. When installation finishes, return to Play. Your worlds and sign-in are kept.")
                    Link("Read game-file transfer instructions", destination: URL(string: "https://github.com/Kelly-Inc/horizon-ios/blob/main/docs/native-client.md#carry-your-game-files-to-a-phone")!)
                }
                Section("Automatic setup or download link") {
                    GameFilesCard(download: download, missing: true, repairing: repairing, retryLoader: retryLoader)
                }
            }
            .navigationTitle("Game files needed")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button("Done") { dismiss() }
                }
            }
        }
        .accessibilityIdentifier("launcher-game-files-recovery")
    }
}
