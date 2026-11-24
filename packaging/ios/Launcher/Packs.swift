import CoreFoundation
import CryptoKit
import Darwin
import SwiftUI
import UIKit

/// What a pack is for, and where the game keeps it.
enum PackKind: String, CaseIterable, Identifiable {
    case resource, shader

    var id: String { rawValue }
    var title: String { self == .resource ? "Resource Packs" : "Shader Packs" }
    var folder: URL {
        GameFolder.documents.appendingPathComponent(self == .resource ? "resourcepacks" : "shaderpacks")
    }
    /// Modrinth's search facets: resource packs, or shader packs in the Iris/OptiFine format the game reads.
    var facets: String {
        self == .resource
            ? #"[["project_type:resourcepack"]]"#
            : #"[["project_type:shader"],["categories:iris","categories:optifine"]]"#
    }
}

/// A project from Modrinth's search.
struct ModrinthProject: Identifiable, Equatable {
    let id: String
    let kind: PackKind
    let title: String
    let summary: String
    let author: String
    let downloads: Int
    let icon: URL?
    let gameVersions: [String]
}

private struct PackFileIntegrity: Sendable {
    private enum Algorithm: Sendable { case sha512, sha1 }
    private let algorithm: Algorithm
    private let expectedHash: String
    let expectedSize: Int64

    init?(metadata: [String: Any]) {
        guard let number = metadata["size"] as? NSNumber, CFGetTypeID(number) != CFBooleanGetTypeID(),
              let size = number as? Int64, size > 0,
              let hashes = metadata["hashes"] as? [String: Any] else { return nil }
        let algorithm: Algorithm = hashes["sha512"] == nil ? .sha1 : .sha512
        let key = algorithm == .sha512 ? "sha512" : "sha1"
        let length = algorithm == .sha512 ? 128 : 40
        guard let hash = (hashes[key] as? String)?.lowercased(), hash.utf8.count == length,
              hash.utf8.allSatisfy({ (48...57).contains($0) || (97...102).contains($0) }) else { return nil }
        self.algorithm = algorithm
        expectedHash = hash
        expectedSize = size
    }

    func matches(_ url: URL) throws -> Bool {
        let file = try FileHandle(forReadingFrom: url)
        defer { try? file.close() }
        guard try file.seekToEnd() == UInt64(expectedSize) else { return false }
        try file.seek(toOffset: 0)
        var sha512 = SHA512()
        var sha1 = Insecure.SHA1()
        var count: Int64 = 0
        while let chunk = try file.read(upToCount: 1 << 20), !chunk.isEmpty {
            try Task.checkCancellation()
            guard Int64(chunk.count) <= expectedSize - count else { return false }
            count += Int64(chunk.count)
            switch algorithm {
            case .sha512: sha512.update(data: chunk)
            case .sha1: sha1.update(data: chunk)
            }
        }
        guard count == expectedSize else { return false }
        let digest = algorithm == .sha512 ? Array(sha512.finalize()) : Array(sha1.finalize())
        return digest.map { String(format: "%02x", $0) }.joined() == expectedHash
    }
}

/// Packs from Modrinth (api.modrinth.com, v2): search, then the newest version made for the game's Minecraft
/// version (else the newest available), preferring its primary downloadable ZIP.
@MainActor
final class PackStore: ObservableObject {
    @Published private(set) var results: [ModrinthProject] = []
    @Published private(set) var searching = false
    @Published private(set) var failed: String?
    /// Project ids being downloaded, with how far along.
    @Published private(set) var downloading: [String: Double] = [:]
    @Published private(set) var installed: [PackKind: [String]] = [:]

    typealias Fetch = @MainActor (URL) async -> Any?
    typealias Transfer = @MainActor (URLRequest, Int64) async throws -> (URL, URLResponse)
    private let fetch: Fetch
    private let transfer: Transfer
    private let folder: (PackKind) -> URL
    private var searchRevision: UInt64 = 0
    init(fetch: @escaping Fetch = { await PackStore.get($0) },
         transfer: @escaping Transfer = { try await PackStore.transferArchive($0, expectedSize: $1) },
         folder: @escaping (PackKind) -> URL = { $0.folder }) {
        self.fetch = fetch
        self.transfer = transfer
        self.folder = folder
        _ = ArchiveStaging.packs
    }

    private static let api = "https://api.modrinth.com/v2"
    // Matches MAX_ARCHIVE_BYTES for the engine's shaders/ ZIP loader.
    private static let maximumShaderArchiveBytes: Int64 = 512 << 20
    // Each page can own a separate store. Final checks must account for packs
    // published by any store while a checksum is being computed.
    private static var installedRevision: UInt64 = 0

    /// Stop at the advertised file size, even when HTTP omits Content-Length.
    nonisolated static func transferArchive(_ request: URLRequest, expectedSize: Int64) async throws -> (URL, URLResponse) {
        var request = request
        request.setValue("identity", forHTTPHeaderField: "Accept-Encoding")
        let (bytes, response) = try await URLSession.shared.bytes(for: request)
        defer { bytes.task.cancel() }
        guard let http = response as? HTTPURLResponse, http.statusCode == 200 else {
            throw PackError("The pack server refused the download.")
        }
        let mismatch = PackError("The downloaded pack did not match Modrinth's checksum or size. Try downloading it again.")
        let encoding = http.value(forHTTPHeaderField: "Content-Encoding")?.lowercased()
        if encoding == nil || encoding == "identity",
           http.expectedContentLength >= 0, http.expectedContentLength != expectedSize {
            throw mismatch
        }
        try Task.checkCancellation()
        let file = ArchiveStaging.packs.nextFile()
        guard FileManager.default.createFile(atPath: file.path, contents: nil) else {
            throw CocoaError(.fileWriteUnknown)
        }
        do {
            let handle = try FileHandle(forWritingTo: file)
            defer { try? handle.close() }
            let blockSize = 64 * 1024
            var block = Data()
            block.reserveCapacity(blockSize)
            var received: Int64 = 0
            for try await byte in bytes {
                guard received < expectedSize else { throw mismatch }
                received += 1
                block.append(byte)
                if block.count == blockSize {
                    try Task.checkCancellation()
                    try handle.write(contentsOf: block)
                    block.removeAll(keepingCapacity: true)
                }
            }
            try Task.checkCancellation()
            guard received == expectedSize else { throw mismatch }
            try handle.write(contentsOf: block)
            try handle.close()
            return (file, response)
        } catch {
            try? FileManager.default.removeItem(at: file)
            throw error
        }
    }

    func search(_ kind: PackKind, query: String) async {
        guard !Task.isCancelled else { return }
        searchRevision &+= 1
        let revision = searchRevision
        searching = true
        defer { if revision == searchRevision { searching = false } }
        var parts = URLComponents(string: "\(Self.api)/search")!
        parts.queryItems = [
            URLQueryItem(name: "query", value: query),
            URLQueryItem(name: "facets", value: kind.facets),
            URLQueryItem(name: "index", value: query.isEmpty ? "downloads" : "relevance"),
            URLQueryItem(name: "limit", value: "30"),
        ]
        guard let url = parts.url else { return }
        let response = await fetch(url)
        guard revision == searchRevision, !Task.isCancelled else { return }
        guard let json = response as? [String: Any], let hits = json["hits"] as? [Any] else {
            results = []
            failed = "Modrinth didn't answer. Check the connection and try again."
            return
        }
        var seen = Set<String>()
        let matches: [ModrinthProject] = hits.compactMap { value in
            guard let hit = value as? [String: Any],
                  let id = hit["project_id"] as? String, !id.isEmpty,
                  let title = hit["title"] as? String, !title.isEmpty,
                  seen.insert(id).inserted else { return nil }
            return ModrinthProject(
                id: id, kind: kind, title: title, summary: hit["description"] as? String ?? "",
                author: hit["author"] as? String ?? "", downloads: hit["downloads"] as? Int ?? 0,
                icon: (hit["icon_url"] as? String).flatMap(URL.init(string:)),
                gameVersions: hit["versions"] as? [String] ?? []
            )
        }
        results = matches
        failed = !hits.isEmpty && matches.isEmpty ? "Modrinth returned unreadable search results." : nil
    }

    /// The installed file and whether an existing copy matched.
    struct Installed {
        let file: String
        let note: String?
        let reused: Bool
    }

    private struct Download {
        let name: String
        let url: URL
        let integrity: PackFileIntegrity
        let note: String?
    }

    private static func verify(_ file: URL, using integrity: PackFileIntegrity) async throws -> Bool {
        let verification = Task.detached(priority: .utility) { try integrity.matches(file) }
        return try await withTaskCancellationHandler {
            try await verification.value
        } onCancel: {
            verification.cancel()
        }
    }

    private static func matchingCopy(of download: Download, in directory: URL) async throws -> URL? {
        let manager = FileManager.default
        let stem = (download.name as NSString).deletingPathExtension
        let numbered = (try? manager.contentsOfDirectory(atPath: directory.path)) ?? []
        let names = [download.name] + numbered.filter { name in
            isNumberedArchive(name, for: stem)
        }.sorted()

        for name in names {
            try Task.checkCancellation()
            let file = directory.appendingPathComponent(name)
            guard let before = try? manager.attributesOfItem(atPath: file.path),
                  before[.type] as? FileAttributeType == .typeRegular,
                  let inode = before[.systemFileNumber] as? NSNumber,
                  (try? await verify(file, using: download.integrity)) == true else { continue }
            try Task.checkCancellation()
            guard let after = try? manager.attributesOfItem(atPath: file.path),
                  after[.type] as? FileAttributeType == .typeRegular,
                  after[.systemFileNumber] as? NSNumber == inode,
                  after[.modificationDate] as? Date == before[.modificationDate] as? Date,
                  after[.size] as? NSNumber == before[.size] as? NSNumber else { continue }
            return file
        }
        return nil
    }

    private static func chooseDownload(_ versions: [[String: Any]], project: ModrinthProject,
                                       version: String) -> Result<Download, PackError> {
        let releases = versions.map { release in
            let files = (release["files"] as? [Any])?.compactMap { $0 as? [String: Any] } ?? []
            let games = (release["game_versions"] as? [Any])?.compactMap { $0 as? String } ?? []
            return (files: files, games: games)
        }
        let matching = releases.filter { $0.games.contains(version) }
        let others = releases.filter { !$0.games.contains(version) }
        let attachmentTypes = ["required-resource-pack", "optional-resource-pack",
                               "sources-jar", "dev-jar", "javadoc-jar", "signature"]
        var missingIntegrity = false
        var oversizedShader = false

        // Keep Modrinth's release order within each compatibility group.
        for release in matching + others {
            let primary = release.files.filter { $0["primary"] as? Bool == true }
            let secondary = release.files.filter { $0["primary"] as? Bool != true }
            for file in primary + secondary {
                guard !attachmentTypes.contains(file["file_type"] as? String ?? ""),
                      let url = (file["url"] as? String).flatMap(URL.init(string:)),
                      url.scheme?.lowercased() == "https", url.host != nil,
                      let name = file["filename"] as? String,
                      !name.contains("/"), !name.contains("\\"), !name.hasPrefix("."),
                      !name.utf8.contains(0),
                      name.lowercased().hasSuffix(".zip") else { continue }
                guard let integrity = PackFileIntegrity(metadata: file) else {
                    missingIntegrity = true
                    continue
                }
                if project.kind == .shader, integrity.expectedSize > maximumShaderArchiveBytes {
                    oversizedShader = true
                    continue
                }
                let made = release.games.last ?? "another version"
                let note = release.games.contains(version) ? nil
                    : "It was made for Minecraft \(made), so it may not look right on \(version)."
                let filename = name.utf8.count <= Int(NAME_MAX)
                    ? name
                    : archiveName(stem: (name as NSString).deletingPathExtension, suffix: ".zip")
                return .success(Download(name: filename, url: url, integrity: integrity, note: note))
            }
        }
        if oversizedShader {
            return .failure(PackError("The shader pack exceeds the engine’s 512 MiB archive limit."))
        }
        return .failure(PackError(missingIntegrity
            ? "Modrinth did not provide a valid checksum and size for this pack."
            : "\(project.title) has no file to download."))
    }

    /// Leave room for the collision suffix on filesystems that limit a name's UTF-8 bytes.
    nonisolated private static func numberedArchive(stem: String, number: Int) -> String {
        archiveName(stem: stem, suffix: " (\(number)).zip")
    }

    nonisolated private static func archiveName(stem: String, suffix: String) -> String {
        let available = Int(NAME_MAX) - suffix.utf8.count
        var prefix = ""
        var bytes = 0
        for character in stem {
            let size = String(character).utf8.count
            guard bytes + size <= available else { break }
            prefix.append(character)
            bytes += size
        }
        return (prefix.isEmpty ? "Pack" : prefix) + suffix
    }

    nonisolated private static func isNumberedArchive(_ name: String, for stem: String) -> Bool {
        guard name.hasSuffix(").zip"), let delimiter = name.range(of: " (", options: .backwards),
              let number = Int(name[delimiter.upperBound...].dropLast(5)), number >= 2 else { return false }
        return name == numberedArchive(stem: stem, number: number) || name == "\(stem) (\(number)).zip"
    }

    /// Commit verified bytes without replacing any path already using the name.
    nonisolated static func publishArchive(_ verified: URL, preferred: URL) throws -> URL {
        let manager = FileManager.default
        let directory = preferred.deletingLastPathComponent()
        let stem = preferred.deletingPathExtension().lastPathComponent
        var destination = preferred
        var source = verified
        var staged: URL?
        var suffix = 2
        defer { if let staged { try? manager.removeItem(at: staged) } }

        while true {
            try Task.checkCancellation()
            if renamex_np(source.path, destination.path, UInt32(RENAME_EXCL)) == 0 {
                return destination
            }
            let failure = errno
            switch failure {
            case EEXIST:
                destination = directory.appendingPathComponent(numberedArchive(stem: stem, number: suffix))
                suffix += 1
            case EXDEV where staged == nil:
                // Keep the cross-volume copy hidden until it can be published atomically.
                let copy = directory.appendingPathComponent(".horizon-pack-\(UUID().uuidString)")
                staged = copy
                try manager.copyItem(at: verified, to: copy)
                source = copy
            default:
                throw NSError(domain: NSPOSIXErrorDomain, code: Int(failure))
            }
        }
    }

    /// Download `project`'s file for the game's Minecraft version into the project's pack folder.
    func install(_ project: ModrinthProject, version: String) async -> Result<Installed, PackError> {
        let cancelled = PackError("The pack download was cancelled.")
        guard !Task.isCancelled else { return .failure(cancelled) }
        guard downloading[project.id] == nil else { return .failure(PackError("This pack is already downloading.")) }
        guard let url = URL(string: "\(Self.api)/project/\(project.id)/version") else {
            return .failure(PackError("Modrinth returned an invalid project reference."))
        }
        downloading[project.id] = 0
        defer { downloading[project.id] = nil }
        let response = await fetch(url)
        guard !Task.isCancelled else { return .failure(cancelled) }
        let records = response as? [Any]
        let versions = records?.compactMap { $0 as? [String: Any] } ?? []
        guard let records, records.isEmpty || !versions.isEmpty else {
            return .failure(PackError("Could not read the pack's versions from Modrinth. Try again."))
        }
        let candidate: Download
        switch Self.chooseDownload(versions, project: project, version: version) {
        case let .success(download): candidate = download
        case let .failure(error): return .failure(error)
        }
        let integrity = candidate.integrity
        let directory = folder(project.kind)
        var request = URLRequest(url: candidate.url)
        request.setValue(Self.agent, forHTTPHeaderField: "User-Agent")
        do {
            let installed = try await Self.matchingCopy(of: candidate, in: directory)
            try Task.checkCancellation()
            if let installed {
                reloadInstalled()
                return .success(Installed(file: installed.lastPathComponent, note: candidate.note, reused: true))
            }
            let (temporary, response) = try await transfer(request, integrity.expectedSize)
            defer { try? FileManager.default.removeItem(at: temporary) }
            try Task.checkCancellation()
            guard (response as? HTTPURLResponse)?.statusCode == 200 else {
                return .failure(PackError("Modrinth refused the download of \(project.title)."))
            }
            let matches = try await Self.verify(temporary, using: integrity)
            try Task.checkCancellation()
            guard matches else {
                return .failure(PackError("The downloaded pack did not match Modrinth's checksum or size. Try downloading it again."))
            }
            try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
            let original = directory.appendingPathComponent(candidate.name)
            while true {
                let revision = Self.installedRevision
                let existing = try await Self.matchingCopy(of: candidate, in: directory)
                try Task.checkCancellation()
                guard revision == Self.installedRevision else { continue }
                if let existing {
                    reloadInstalled()
                    return .success(Installed(file: existing.lastPathComponent, note: candidate.note, reused: true))
                }
                let destination = try Self.publishArchive(temporary, preferred: original)
                Self.installedRevision &+= 1
                reloadInstalled()
                return .success(Installed(file: destination.lastPathComponent, note: candidate.note, reused: false))
            }
        } catch {
            return .failure(Task.isCancelled ? cancelled : (error as? PackError ?? PackError(error.localizedDescription)))
        }
    }

    /// The zips and folders in each pack folder.
    func reloadInstalled() {
        for kind in PackKind.allCases {
            let directory = folder(kind)
            let names = (try? FileManager.default.contentsOfDirectory(atPath: directory.path)) ?? []
            installed[kind] = names
                .filter { name in
                    !name.hasPrefix(".") && Self.isPack(directory.appendingPathComponent(name), kind: kind)
                }
                .sorted { $0.localizedCaseInsensitiveCompare($1) == .orderedAscending }
        }
    }

    func remove(_ name: String, kind: PackKind) -> String? {
        do {
            try FileManager.default.removeItem(at: folder(kind).appendingPathComponent(name))
            Self.installedRevision &+= 1
            reloadInstalled()
            return nil
        } catch { return error.localizedDescription }
    }

    private static func isPack(_ url: URL, kind: PackKind) -> Bool {
        let keys: Set<URLResourceKey> = [.isDirectoryKey, .isRegularFileKey]
        func type(of path: URL) -> URLResourceValues? {
            try? path.resolvingSymlinksInPath().resourceValues(forKeys: keys)
        }

        let entry = type(of: url)
        if entry?.isRegularFile == true {
            let ext = url.pathExtension.lowercased()
            return ext == "zip" || (kind == .shader && ext == "hshader")
        }
        guard entry?.isDirectory == true else { return false }

        switch kind {
        case .resource:
            return type(of: url.appendingPathComponent("pack.mcmeta"))?.isRegularFile == true
        case .shader:
            return type(of: url.appendingPathComponent("shaders"))?.isDirectory == true
                || type(of: url.appendingPathComponent("horizon-shaders.toml"))?.isRegularFile == true
        }
    }

    /// Modrinth asks every client to name itself.
    private static let agent = "Kelly-Inc/horizon-ios launcher (github.com/Kelly-Inc/horizon-ios)"

    private static func get(_ url: URL) async -> Any? {
        var request = URLRequest(url: url)
        request.setValue(agent, forHTTPHeaderField: "User-Agent")
        guard let (data, response) = try? await URLSession.shared.data(for: request),
              (response as? HTTPURLResponse)?.statusCode == 200
        else { return nil }
        return try? JSONSerialization.jsonObject(with: data)
    }
}

struct PackError: Error {
    let message: String
    init(_ message: String) { self.message = message }
}

/// Which packs the game uses: `resourcePacks` in options.txt (`file/<name>` entries, the last on top) and the
/// shader pack (`shader-pack`, config/iris.properties).
@MainActor
final class PackChoices: ObservableObject {
    @Published private(set) var resourcePacks: [String] = []
    @Published private(set) var shaderPack: String?
    @Published private(set) var shaderStatus: String?
    @Published private(set) var shaderNotes: [String] = []
    @Published private(set) var shaderPending = false

    typealias Request = @MainActor (String) async -> [String: Any]
    private let request: Request
    private let packExists: (String, PackKind) -> Bool
    private var shaderKeys: [String: String] = [:]
    private var selectionReadIssues: [PackKind: String] = [:]
    // The engine has one shader slot even when SwiftUI recreates this page.
    private static var latestShaderIntent: UUID?
    private static var locked = false
    private static var waiters: [CheckedContinuation<Void, Never>] = []
    @Published private(set) var issue: String?

    init(request: @escaping Request = { await Bridge.send($0) },
         packExists: @escaping (String, PackKind) -> Bool = { name, kind in
             FileManager.default.fileExists(atPath: kind.folder.appendingPathComponent(name).path)
         }) {
        self.request = request
        self.packExists = packExists
    }

    private func acquire() async {
        if !Self.locked { Self.locked = true; return }
        await withCheckedContinuation { Self.waiters.append($0) }
    }
    private func release() {
        if Self.waiters.isEmpty { Self.locked = false }
        else { Self.waiters.removeFirst().resume() }
    }

    func load() async {
        await acquire()
        defer { release() }
        guard !Task.isCancelled else { return }
        let error = await reload()
        if !Task.isCancelled { issue = error }
    }

    /// Observe engine changes while this pack kind is visible, including reload rollback.
    func followSelection(_ kind: PackKind, interval: Duration = .seconds(1)) async {
        while !Task.isCancelled {
            do { try await Task.sleep(for: interval) }
            catch { return }
            await acquire()
            guard !Task.isCancelled else {
                release()
                return
            }
            let error = kind == .resource ? await readResources() : await readShaders(statusOnly: true)
            release()
            guard !Task.isCancelled else { return }
            if let error {
                if issue != error { issue = error }
            } else if let previous = selectionReadIssues[kind], issue == previous {
                issue = nil
            }
            selectionReadIssues[kind] = error
        }
    }

    private func readResources() async -> String? {
        let answer = await request("options-get resourcePacks")
        guard !Task.isCancelled else { return nil }
        if let error = answer.error { return error }
        guard let packs = ((answer["options"] as? [String: Any])?["resourcePacks"] as? [String: Any])?["value"] as? [String] else {
            return "The active resource packs could not be read. Try again."
        }
        if resourcePacks != packs { resourcePacks = packs }
        return nil
    }

    private func readShaders(
        requiringKnownSelectionFor name: String? = nil, statusOnly: Bool = false
    ) async -> String? {
        let shaders = await request(statusOnly ? "shader-pack-status" : "shader-pack list")
        guard !Task.isCancelled else { return nil }
        if let error = shaders.error { return error }
        guard let selected = shaders["selected"], selected is NSNull || selected is String else {
            return "The selected shader pack could not be read. Try again."
        }
        var keys: [String: String] = [:]
        for case let pack as [String: Any] in shaders["packs"] as? [Any] ?? [] {
            if let name = pack["name"] as? String, let key = pack["key"] as? String {
                keys[name] = key
            }
        }
        let selection = selected as? String
        if statusOnly, let selection, selection.hasPrefix("horizon:"),
           selection != shaderPack, !shaderKeys.values.contains(selection) {
            // Status replies omit the mapping from native shader keys to filenames.
            return await readShaders()
        }
        // A stale selection is only ambiguous when the engine also cannot
        // identify the target file. A listed, different pack is safe to edit.
        if let name, let selection, selection.hasPrefix("horizon:"),
           !keys.values.contains(selection), keys[name] == nil {
            return "The active shader file could not be identified. Try again."
        }
        if !statusOnly { shaderKeys = keys }
        let runtime = shaders["runtime"] as? [String: Any]
        let programs = runtime?["programs"] as? [String]
        let pending = selection != nil && programs != nil && runtime?["pending"] as? Bool == true
        var notes: [String] = []
        var status: String?
        if selection != nil {
            if let runtime, let programs {
                if pending {
                    status = "Preparing shader pack. Normal rendering is active."
                } else {
                    status = programs.isEmpty ? "No supported effects loaded. Normal rendering is active."
                        : "Running \(programs.count) shader programs."
                }
                notes = runtime["notes"] as? [String] ?? []
            } else {
                status = "Selected. Open a world to load this pack, then return here for its status."
            }
        } else if let failure = shaders["last_disabled"] as? [String: Any],
                  let name = failure["pack"] as? String {
            status = "\(name) was turned off because it could not load."
            notes = failure["notes"] as? [String] ?? []
        }
        if shaderPack != selection { shaderPack = selection }
        if shaderPending != pending { shaderPending = pending }
        if shaderStatus != status { shaderStatus = status }
        if shaderNotes != notes { shaderNotes = notes }
        return nil
    }

    private func reload() async -> String? {
        let resources = await readResources()
        guard !Task.isCancelled else { return nil }
        let shaders = await readShaders()
        if !Task.isCancelled {
            selectionReadIssues[.resource] = resources
            selectionReadIssues[.shader] = shaders
        }
        return resources ?? shaders
    }

    func isOn(_ name: String, kind: PackKind) -> Bool {
        if kind == .resource { return resourcePacks.contains("file/\(name)") }
        guard let shaderPack else { return false }
        return shaderPack == name || shaderPack == shaderKeys[name]
    }

    func beginShaderSelection(for kind: PackKind) -> UUID? {
        guard kind == .shader else { return nil }
        let intent = UUID()
        Self.latestShaderIntent = intent
        return intent
    }

    private func isCurrent(_ shaderIntent: UUID?) -> Bool {
        shaderIntent == nil || shaderIntent == Self.latestShaderIntent
    }

    /// Each mutation reads confirmed state while holding the queue lock.
    func set(_ name: String, kind: PackKind, on: Bool, shaderIntent: UUID? = nil) async -> String? {
        guard !Task.isCancelled else { return nil }
        let intent = shaderIntent ?? beginShaderSelection(for: kind)
        if case let .failure(error) = await changeSelection(name, kind: kind, on: on, shaderIntent: intent) {
            return error.message
        }
        return nil
    }

    /// A finished shader download may only activate the selection made when it began.
    func enableDownloaded(_ name: String, kind: PackKind, shaderIntent: UUID?) async -> Result<Bool, PackError> {
        guard kind == .resource || shaderIntent != nil else { return .success(false) }
        return await changeSelection(name, kind: kind, on: true, shaderIntent: shaderIntent)
    }

    private func changeSelection(
        _ name: String, kind: PackKind, on: Bool, shaderIntent: UUID?
    ) async -> Result<Bool, PackError> {
        await acquire()
        defer { release() }
        guard !Task.isCancelled, isCurrent(shaderIntent) else { return .success(false) }
        let error = await setLocked(name, kind: kind, on: on, shaderIntent: shaderIntent)
        guard !Task.isCancelled, isCurrent(shaderIntent) else { return .success(false) }
        issue = error
        if let error { return .failure(PackError(error)) }
        return .success(true)
    }

    private func setLocked(
        _ name: String, kind: PackKind, on: Bool, shaderIntent: UUID? = nil,
        requireSuccessfulWrite: Bool = false
    ) async -> String? {
        let readError = kind == .resource ? await readResources() : await readShaders(requiringKnownSelectionFor: on ? nil : name)
        guard !Task.isCancelled, isCurrent(shaderIntent) else { return nil }
        if let readError { return readError }
        // A queued toggle can run after deletion, even though its row is gone.
        if on && !packExists(name, kind) { return "That pack is no longer installed." }
        // Off is a global engine command for shaders. Never use it to
        // disable a different shader that became active while this waited.
        guard isOn(name, kind: kind) != on else { return nil }
        let answer: [String: Any]
        switch kind {
        case .resource:
            var packs = resourcePacks.filter { $0 != "file/\(name)" }
            if on { packs.append("file/\(name)") }
            guard let data = try? JSONSerialization.data(withJSONObject: packs),
                  let list = String(data: data, encoding: .utf8) else { return "That pack name can't be saved." }
            answer = await request("options-set resourcePacks \(list)")
        case .shader:
            let key = shaderKeys[name] ?? name
            guard let data = try? JSONSerialization.data(withJSONObject: ["name": key, "enabled": on]),
                  let selection = String(data: data, encoding: .utf8) else { return "That shader name can't be saved." }
            answer = await request("shader-pack-set \(selection)")
        }
        guard !Task.isCancelled else { return nil }
        // Read back after errors, but deletion requires a confirmed write.
        let writeError = answer.error
        let error = kind == .resource ? await readResources() : await readShaders(requiringKnownSelectionFor: on ? nil : name)
        let selected = answer["selected"]
        let confirmed = answer.acknowledged || (kind == .shader && (selected is String || selected is NSNull))
        if requireSuccessfulWrite, writeError != nil || !confirmed {
            return writeError ?? "The game did not confirm the pack change."
        }
        if let error { return writeError ?? error }
        guard isOn(name, kind: kind) == on else {
            return writeError ?? "The pack change was not confirmed. Try again."
        }
        return nil
    }

    /// Hold the same lock through disable + confirmation + deletion. Failed/unknown disable state
    /// leaves the file intact. The closure allows a focused test without changing real pack folders.
    func remove(_ name: String, kind: PackKind, delete: () -> String?) async -> String? {
        guard !Task.isCancelled else { return nil }
        // Deleting a file does not replace a pending selection of another
        // shader. A queued activation of this file still checks that it exists.
        await acquire()
        defer { release() }
        guard !Task.isCancelled else { return nil }
        let readError = kind == .resource ? await readResources() : await readShaders(requiringKnownSelectionFor: name)
        guard !Task.isCancelled else { return nil }
        if let readError { issue = readError; return readError }
        if isOn(name, kind: kind),
           let error = await setLocked(name, kind: kind, on: false, requireSuccessfulWrite: true) {
            if !Task.isCancelled { issue = error }
            return error
        }
        guard !Task.isCancelled else { return nil }
        let error = delete()
        issue = error
        return error
    }

}

// MARK: - The page

struct PacksPage: View {
    @ObservedObject var link: GameLink
    @StateObject private var store = PackStore()
    @StateObject private var choices = PackChoices()
    @State private var kind = PackKind.resource
    @State private var query = ""
    @State private var notice: String?
    @State private var requestedDownloads: Set<String> = []

    var body: some View {
        let visibleResults = store.searching ? [] : store.results.filter { $0.kind == kind }
        VStack(alignment: .leading, spacing: 16) {
            Picker("Kind", selection: $kind) {
                ForEach(PackKind.allCases) { Text($0.title).tag($0) }
            }
            .pickerStyle(.segmented)

            installedSection
            if let issue = choices.issue {
                Label(issue, systemImage: "exclamationmark.triangle").font(.footnote).foregroundStyle(Theme.warn)
            }

            if kind == .shader, let status = choices.shaderStatus {
                Text(status).font(Theme.sans(12)).foregroundStyle(Theme.text)
                ForEach(Array(choices.shaderNotes.prefix(8).enumerated()), id: \.offset) { _, note in
                    Text(note).font(Theme.sans(11)).foregroundStyle(Theme.warn)
                        .textSelection(.enabled)
                }
            }

            SectionHeader(title: "Find on Modrinth")
            Field(title: "SEARCH", placeholder: kind == .resource ? "Faithful, Stay True…" : "Complementary, BSL…",
                  text: $query, keyboard: .default)
                .onSubmit { Task { await store.search(kind, query: query) } }
            if kind == .shader {
                Text("Compatibility varies by pack and settings. Check the runtime notes for unsupported features or loading errors.")
                    .font(Theme.sans(11)).foregroundStyle(Theme.textFaint)
            }
            if store.searching {
                ProgressView().tint(Theme.accentText).frame(maxWidth: .infinity).padding(24)
            } else if let failed = store.failed {
                Label(failed, systemImage: "wifi.exclamationmark").font(Theme.sans(12)).foregroundStyle(Theme.warn)
                SecondaryButton(title: "Try again", icon: "arrow.clockwise") {
                    Task { await store.search(kind, query: query) }
                }
            } else if visibleResults.isEmpty {
                EmptyNote(icon: "magnifyingglass", text: "No packs found.")
            }
            VStack(spacing: 0) {
                ForEach(visibleResults) { project in
                    ProjectRow(project: project,
                               progress: store.downloading[project.id] ?? (requestedDownloads.contains(project.id) ? 0 : nil),
                               version: link.version) {
                        guard requestedDownloads.insert(project.id).inserted else { return }
                        let intent = choices.beginShaderSelection(for: project.kind)
                        Task {
                            defer { requestedDownloads.remove(project.id) }
                            await download(project, shaderIntent: intent)
                        }
                    }
                    if project != visibleResults.last {
                        Divider().overlay(Theme.border).padding(.leading, 70)
                    }
                }
            }
            .card()
            Text("Packs come from modrinth.com and their authors. Horizon is not affiliated with Modrinth.")
                .font(Theme.sans(11)).foregroundStyle(Theme.textFaint)
        }
        .task(id: kind) {
            store.reloadInstalled()
            await store.search(kind, query: query)
        }
        .task(id: link.canList) {
            if link.canList { await choices.load() }
        }
        .task(id: link.canList ? kind : nil) {
            if link.canList { await choices.followSelection(kind) }
        }
        .alert("Packs", isPresented: Binding(get: { notice != nil }, set: { if !$0 { notice = nil } })) {
            Button("OK", role: .cancel) {}
        } message: {
            Text(notice ?? "")
        }
    }

    @ViewBuilder private var installedSection: some View {
        let rowKind = kind
        let names = store.installed[rowKind] ?? []
        SectionHeader(title: "Installed")
        if names.isEmpty {
            EmptyNote(icon: kind == .resource ? "square.stack.3d.up" : "sun.max",
                      text: "None yet. Find one below, or drop a .zip into Horizon's \(kind == .resource ? "resourcepacks" : "shaderpacks") folder in Files.")
        } else {
            VStack(spacing: 0) {
                ForEach(names, id: \.self) { name in
                    HStack(spacing: 12) {
                        Text(name).font(Theme.sans(14)).foregroundStyle(Theme.text).lineLimit(2)
                        Spacer(minLength: 8)
                        Toggle("", isOn: Binding(
                            get: { choices.isOn(name, kind: rowKind) },
                            set: { on in
                                Haptics.tap()
                                let intent = choices.beginShaderSelection(for: rowKind)
                                Task {
                                    if let error = await choices.set(name, kind: rowKind, on: on, shaderIntent: intent) {
                                        notice = error
                                    }
                                }
                            }
                        ))
                        .labelsHidden()
                        .tint(Theme.accent)
                        .disabled(!link.canList)
                    }
                    .padding(.horizontal, 14)
                    .frame(minHeight: 50)
                    .contextMenu {
                        Button(role: .destructive) {
                            Task {
                                if let error = await choices.remove(name, kind: rowKind, delete: {
                                    store.remove(name, kind: rowKind)
                                }) { notice = error }
                            }
                        } label: { Label("Delete", systemImage: "trash") }
                    }
                    if name != names.last {
                        Divider().overlay(Theme.border).padding(.leading, 14)
                    }
                }
            }
            .card()
            Text(link.canList
                ? "Switch a pack on to use it; hold one to delete it."
                : "Packs can be switched on in a moment.")
                .font(Theme.sans(11)).foregroundStyle(Theme.textFaint)
        }
    }

    private func download(_ project: ModrinthProject, shaderIntent: UUID?) async {
        guard !Task.isCancelled else { return }
        Haptics.tap()
        let result = await store.install(project, version: link.version)
        guard !Task.isCancelled else { return }
        switch result {
        case let .success(installed):
            Haptics.done()
            let name = installed.file
            let status = installed.reused ? "\(name) is already installed" : "\(name) is downloaded"
            var said: String
            if link.canList {
                let activation = await choices.enableDownloaded(name, kind: project.kind, shaderIntent: shaderIntent)
                guard !Task.isCancelled else { return }
                switch activation {
                case .success(true): said = "\(status) and on."
                case .success(false): said = "\(status)."
                case let .failure(error): said = "\(status). Could not switch it on. \(error.message)"
                }
            } else {
                said = "\(status). Switch it on in a moment."
            }
            if let note = installed.note {
                said += " " + note
            }
            notice = said
        case let .failure(error):
            Haptics.failed()
            notice = error.message
        }
    }
}

private struct ProjectRow: View {
    let project: ModrinthProject
    let progress: Double?
    let version: String
    let download: () -> Void

    var body: some View {
        HStack(alignment: .top, spacing: 12) {
            AsyncImage(url: project.icon) { image in
                image.resizable().interpolation(.none).scaledToFill()
            } placeholder: {
                Theme.surfaceSunken
            }
            .frame(width: 46, height: 46)
            .clipShape(RoundedRectangle(cornerRadius: 8))
            .overlay(RoundedRectangle(cornerRadius: 8).strokeBorder(Theme.border))
            VStack(alignment: .leading, spacing: 3) {
                Text(project.title).font(Theme.sans(15, .semiBold)).foregroundStyle(Theme.text).lineLimit(1)
                Text(project.summary).font(Theme.sans(11)).foregroundStyle(Theme.textMuted).lineLimit(2)
                HStack(spacing: 6) {
                    Text("by \(project.author)")
                    Text("·")
                    Text("\(project.downloads.formatted(.number.notation(.compactName))) downloads")
                    if !project.gameVersions.contains(version) {
                        Text("·")
                        Text("not tagged \(version)").foregroundStyle(Theme.warn)
                    }
                }
                .font(Theme.mono(9)).foregroundStyle(Theme.textFaint).lineLimit(1)
            }
            Spacer(minLength: 4)
            Group {
                if progress != nil {
                    ProgressView().tint(Theme.accentText)
                } else {
                    Button(action: download) {
                        Image(systemName: "arrow.down.circle.fill").font(.system(size: 26))
                            .foregroundStyle(Theme.accentBright)
                    }
                    .accessibilityLabel("Download \(project.title)")
                }
            }
            .frame(width: 44, height: 44)
        }
        .padding(12)
    }
}
