import Darwin
import SwiftUI
import UIKit

/// Keeps the exported file alive for its picker.
final class DiagnosticExport: Identifiable, Sendable {
    let url: URL

    fileprivate init(directory: URL) {
        url = directory.appendingPathComponent("Horizon-diagnostics.txt")
    }

    deinit {
        try? FileManager.default.removeItem(at: url.deletingLastPathComponent())
    }
}

/// Only diagnostic text is exported; never account stores, options or game data.
enum Diagnostics {
    static let limit = 1024 * 1024
    private static let entryLimit = 32 * 1024
    private static let lock = NSLock()
    static var directory: URL {
        FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("HorizonDiagnostics", isDirectory: true)
    }
    private static let sensitive = [
        "accesstoken", "refreshtoken", "idtoken", "user code",
        "access_token", "refresh_token", "id_token", "token\"",
        "authorization", "bearer", "password", "secret", "cookie", "xbl3.0",
        "device_code", "user_code", "sessionid", "session_id", "api_key", "accesskey",
        "devicecode", "usercode", "rpsticket", "usertokens",
    ]
    // Each marker is a literal required by its pattern. Avoid expensive searches
    // to keep ordinary status lines cheap.
    private static let patterns: [(regex: NSRegularExpression, marker: String?, minimumBytes: Int)] = [
        (#"https?://[^\s]+"#, "://", 0),
        (#"\beyJ[A-Za-z0-9_.-]+"#, "eyj", 4),
        (#"[A-Za-z0-9_./+=-]{129,}"#, nil, 129),
        (#"[A-Z0-9._%+-]+@[A-Z0-9.-]+\.[A-Z]{2,}"#, "@", 0),
        (#"\b(?:\d{1,3}\.){3}\d{1,3}(?::\d+)?\b"#, ".", 7),
        (#"\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b"#, "-", 36),
        (#"/(?:Users|var/mobile)/[^\s]+"#, "/", 0),
    ].compactMap { pattern, marker, minimumBytes in
        guard let regex = try? NSRegularExpression(pattern: pattern, options: .caseInsensitive) else { return nil }
        return (regex, marker, minimumBytes)
    }
    private static let ipv6Endpoint = try? NSRegularExpression(
        pattern: #"(?<![a-z0-9_:.%])(?:\[([0-9a-f:.]{2,45})(?:%[a-z0-9_.~-]+)?\](?::[0-9]+)?|(?=[0-9a-f]{0,4}:)([0-9a-f:.]{1,44}[0-9a-f:])(?:%[a-z0-9_.~-]+)?)(?![a-z0-9_:%]|\.[a-z0-9_])"#,
        options: .caseInsensitive
    )

    private static func redactIPv6(_ line: String) -> String {
        guard line.contains(":"), let ipv6Endpoint else { return line }
        let matches = ipv6Endpoint.matches(in: line, range: NSRange(line.startIndex..., in: line))
        var redacted = line
        for match in matches.reversed() {
            let addressRange = match.range(at: 1).location == NSNotFound
                ? match.range(at: 2) : match.range(at: 1)
            guard let address = Range(addressRange, in: line),
                  let endpoint = Range(match.range, in: redacted) else { continue }
            // Validate the candidate so times and other colon-separated text stay readable.
            var parsed = in6_addr()
            if String(line[address]).withCString({ inet_pton(AF_INET6, $0, &parsed) }) == 1 {
                redacted.replaceSubrange(endpoint, with: "[redacted]")
            }
        }
        return redacted
    }

    private static let credentialField = try? NSRegularExpression(
        pattern: #"(?:token|(?<![a-z0-9_])(?:uhs|api[-_ ]?key))['"]?\s*[:=]"#
    )
    private static let entryStart = try? NSRegularExpression(
        pattern: #"^(?:\[\d{2}:\d{2}:\d{2}\] \[[^\r\n]+/(?:TRACE|DEBUG|INFO|WARN|ERROR)\]: |\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2}) )"#
    )
    private static func beginsEntry(_ line: String) -> Bool {
        guard let first = line.utf8.first, first == 91 || (48...57).contains(first) else {
            return false
        }
        return entryStart?.firstMatch(in: line, range: NSRange(line.startIndex..., in: line)) != nil
    }
    private static let sensitiveMessage = "[redacted sensitive log message]"

    private static func containsCredential(_ text: String) -> Bool {
        let lowercase = text.lowercased()
        return sensitive.contains(where: { lowercase.contains($0) }) ||
            ((lowercase.contains("token") || lowercase.contains("uhs") || lowercase.contains("key")) && credentialField?.firstMatch(
                in: lowercase, range: NSRange(lowercase.startIndex..., in: lowercase)
            ) != nil)
    }

    static func redact(_ text: String) -> String {
        guard text.utf8.count <= entryLimit else { return "[oversized log entry omitted]" }
        if containsCredential(text) {
            return sensitiveMessage
        }
        return text.components(separatedBy: .newlines).map { line in
            let lowercaseLine = line.lowercased()
            let bytes = line.utf8.count
            var safe = redactIPv6(line)
            for rule in patterns where bytes >= rule.minimumBytes {
                if let marker = rule.marker, !lowercaseLine.contains(marker) { continue }
                safe = rule.regex.stringByReplacingMatches(
                    in: safe, range: NSRange(safe.startIndex..., in: safe), withTemplate: "[redacted]"
                )
            }
            return safe
        }.joined(separator: "\n")
    }
    static func bounded(_ text: String, maximum: Int = limit) -> Data {
        var bytes = Data(text.utf8)
        if bytes.count > maximum {
            bytes = bytes.prefix(maximum - 32)
            // Repair clipped UTF-8 at the boundary.
            bytes = Data((String(decoding: bytes, as: UTF8.self) + "\n[log shortened]\n").utf8)
        }
        return bytes
    }
    static func record(_ message: String, folder: URL = directory) {
        lock.lock(); defer { lock.unlock() }
        do {
            try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
            let current = folder.appendingPathComponent("launcher.log")
            let previous = folder.appendingPathComponent("launcher-previous.log")
            // Indent continuation lines.
            let safe = redact(message).replacingOccurrences(of: "\n", with: "\n  ")
            let data = bounded("\(ISO8601DateFormatter().string(from: Date())) \(safe)\n", maximum: entryLimit)
            if !FileManager.default.fileExists(atPath: current.path) { try Data().write(to: current) }
            let file = try FileHandle(forUpdating: current)
            defer { try? file.close() }
            let end = try file.seekToEnd()
            if end + UInt64(data.count) + 1 > UInt64(limit) {
                let tail = try read(current)
                let retained = exportTail(redactExport(tail), maximum: limit)
                if !retained.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
                    do {
                        try Data(retained.utf8).write(to: previous, options: .atomic)
                    } catch {
                        // Keep logging if rotation fails.
                        let recent = exportTail(retained, maximum: limit / 2)
                        var fallback = Data(recent.utf8)
                        if !fallback.isEmpty, fallback.last != 0x0A { fallback.append(0x0A) }
                        fallback.append(data)
                        try fallback.write(to: current, options: .atomic)
                        return
                    }
                }
                try data.write(to: current, options: .atomic)
                return
            }
            do {
                if end > 0 {
                    try file.seek(toOffset: end - 1)
                    if try file.read(upToCount: 1)?.first != 0x0A {
                        try file.write(contentsOf: Data([0x0A]))
                    }
                }
                try file.write(contentsOf: data)
            } catch { try file.truncate(atOffset: end) }
        } catch { /* Logging must not interrupt gameplay. */ }
    }

    static func read(_ url: URL) throws -> String {
        let file = try FileHandle(forReadingFrom: url)
        defer { try? file.close() }
        return try read(file)
    }

    private static func read(_ file: FileHandle) throws -> String {
        let length = try file.seekToEnd()
        let offset = length > UInt64(limit) ? length - UInt64(limit) : 0
        let startsAtLine: Bool
        if offset == 0 {
            try file.seek(toOffset: 0)
            startsAtLine = true
        } else {
            try file.seek(toOffset: offset - 1)
            startsAtLine = try file.read(upToCount: 1)?.first == 0x0A
        }
        let data = try file.read(upToCount: limit) ?? Data()
        let text = String(decoding: data, as: UTF8.self)
        guard length > UInt64(limit) else {return text}
        // Drop orphaned continuations.
        let lines = text.components(separatedBy: "\n")
        let completeLines = startsAtLine ? lines[...] : lines.dropFirst()
        guard let first = completeLines.firstIndex(where: beginsEntry) else {return ""}
        return completeLines[first...].joined(separator: "\n")
    }
    static func redactExport(_ text: String) -> String {
        var output = ""
        var entry = ""
        var stamp = ""
        var hasOutput = false
        var hasEntry = false
        var hidingContinuation = false

        func flushEntry() {
            guard hasEntry else { return }
            if hasOutput { output += "\n" }
            // Detect folded credentials across continuation lines.
            output += entry.contains("\n") && containsCredential(entry) ? stamp + sensitiveMessage : entry
            hasOutput = true
            entry = ""
            hasEntry = false
        }

        text.enumerateLines { line, _ in
            if beginsEntry(line) {
                flushEntry()
                // Keep redacted messages recognizable on future exports.
                stamp = String(line.prefix { $0 != " " }) + (line.hasPrefix("[") ? " [Diagnostics/INFO]: " : " ")
                hidingContinuation = false
            }
            guard !hidingContinuation else { return }

            let oversized = line.utf8.count > entryLimit
            let safe = oversized ? "[oversized log line omitted]" : (line.isEmpty ? "" : redact(line))
            if oversized || safe.contains(sensitiveMessage) {
                // Replace the message but retain its timestamp.
                entry = stamp + (oversized ? safe : sensitiveMessage)
                hasEntry = true
                hidingContinuation = true
                return
            }
            if hasEntry { entry += "\n" }
            entry += safe
            hasEntry = true
        }
        flushEntry()
        if hasOutput, text.last?.isNewline == true { output += "\n" }
        return output
    }

    /// Trim redacted text at line boundaries.
    static func exportTail(_ text: String, maximum: Int) -> String {
        let data = Data(text.utf8)
        guard data.count > maximum else {return text}
        let marker = "[older lines omitted]\n"
        guard maximum > marker.utf8.count else {return ""}
        let suffixLength = maximum - marker.utf8.count
        let suffix = String(decoding: data.suffix(suffixLength), as: UTF8.self)
        if data[data.count - suffixLength - 1] == 0x0A {
            return marker + suffix
        }
        guard let newline = suffix.firstIndex(of: "\n") else {return marker}
        return marker + suffix[suffix.index(after: newline)...]
    }

    /// Export the readable logs independently.
    private static func exportSnapshot(_ url: URL) -> String {
        let unavailable = "Log unavailable during export.\n"
        do {
            guard try url.resourceValues(forKeys: [.isRegularFileKey]).isRegularFile == true else {
                return unavailable
            }
            return try read(url)
        } catch let error as CocoaError where error.code == .fileNoSuchFile || error.code == .fileReadNoSuchFile {
            return "No log recorded yet.\n"
        } catch {
            // File errors can contain private paths. Keep the report text generic.
            return unavailable
        }
    }

    private static func launcherSnapshots(_ folder: URL) -> (previous: String, current: String) {
        // Capture both logs under the writer lock to avoid mixing rotations. Redact them after unlocking.
        lock.lock()
        defer { lock.unlock() }
        return (
            exportSnapshot(folder.appendingPathComponent("launcher-previous.log")),
            exportSnapshot(folder.appendingPathComponent("launcher.log"))
        )
    }

    private static func gameSnapshots(_ gameHome: URL) -> (previous: String, current: String) {
        let previous = gameHome.appendingPathComponent("logs/previous.log")
        let current = gameHome.appendingPathComponent("logs/latest.log")
        guard (try? current.resourceValues(forKeys: [.isRegularFileKey]).isRegularFile) == true,
              let handle = try? FileHandle(forReadingFrom: current) else {
            return (exportSnapshot(previous), exportSnapshot(current))
        }
        defer { try? handle.close() }
        // Rust holds the matching exclusive flock during archive replacement
        // and truncation. Wait briefly off the UI thread.
        let deadline = ProcessInfo.processInfo.systemUptime + 0.5
        while flock(handle.fileDescriptor, LOCK_SH | LOCK_NB) != 0 {
            let failure = errno
            guard failure == EWOULDBLOCK || failure == EINTR else {
                let message = "Game logs unavailable during export.\n"
                return (message, message)
            }
            if ProcessInfo.processInfo.systemUptime >= deadline {
                let message = "Game logs are rotating. Try exporting again.\n"
                return (message, message)
            }
            Thread.sleep(forTimeInterval: 0.025)
        }
        let previousSnapshot = exportSnapshot(previous)
        let currentSnapshot = (try? read(handle)) ?? "Log unavailable during export.\n"
        return (previousSnapshot, currentSnapshot)
    }

    private static let exportDirectory: URL = {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("HorizonDiagnosticExports", isDirectory: true)
        // Clean once before creating a snapshot.
        try? FileManager.default.removeItem(at: root)
        return root
    }()

    static func gameLogHome(fromEngine answer: [String: Any]) -> URL? {
        guard answer.error == nil, let path = answer["path"] as? String,
              path.hasPrefix("/"), !path.utf8.contains(0) else {
            return nil
        }
        return URL(fileURLWithPath: path, isDirectory: true)
    }

    static func gameLogHome(override: String?, dataDirectory: URL) -> URL {
        // The engine treats an empty HORIZON_HOME as unset.
        if let override, !override.isEmpty {
            return URL(fileURLWithPath: override, isDirectory: true)
        }
        return dataDirectory.appendingPathComponent("horizon", isDirectory: true)
    }

    static func export(folder: URL = directory, gameFolder: URL? = nil,
                       gameLogMayBeIncomplete: Bool = false) throws -> DiagnosticExport {
        let fm = FileManager.default
        var output = "Horizon mobile diagnostics\nVersion: \(Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") ?? "unknown") Build: \(Bundle.main.object(forInfoDictionaryKey: "CFBundleVersion") ?? "unknown")\niOS: \(ProcessInfo.processInfo.operatingSystemVersionString)\nSensitive lines and common identifiers are redacted. Review before sharing.\n"
        if gameLogMayBeIncomplete {
            output += "The game log writer could not confirm that recent messages were saved.\n"
        }
        let gameHome = gameFolder ?? gameLogHome(
            override: ProcessInfo.processInfo.environment["HORIZON_HOME"],
            dataDirectory: fm.urls(for: .applicationSupportDirectory, in: .userDomainMask).first ?? fm.temporaryDirectory
        )
        let launcher = launcherSnapshots(folder)
        let game = gameSnapshots(gameHome)
        let entries = [
            ("Launcher previous", launcher.previous),
            ("Launcher current", launcher.current),
            ("Game previous", game.previous),
            ("Game current", game.current),
        ]
        let headings = entries.map { "\n--- \($0.0) ---\n" }
        let overhead = output.utf8.count + headings.reduce(0) { $0 + $1.utf8.count }
        let logs = entries.map { redactExport($0.1) }
        let sizes = logs.map { $0.utf8.count }
        // Small logs use only the space they need. Divide the remaining space
        // among the larger logs so early, empty sources can't crowd out failures.
        var remaining = max(0, 4 * limit - overhead)
        var budgets = Array(repeating: 0, count: entries.count)
        for (offset, index) in sizes.indices.sorted(by: { sizes[$0] < sizes[$1] }).enumerated() {
            let budget = min(sizes[index], remaining / (entries.count - offset))
            budgets[index] = budget
            remaining -= budget
        }
        for index in entries.indices {
            output += headings[index]
            output += exportTail(logs[index], maximum: budgets[index])
        }
        let exportDir = exportDirectory.appendingPathComponent(UUID().uuidString, isDirectory: true)
        try fm.createDirectory(at: exportDir, withIntermediateDirectories: true)
        let snapshot = DiagnosticExport(directory: exportDir)
        try bounded(output, maximum: 4 * limit).write(to: snapshot.url, options: .atomic)
        return snapshot
    }
}

struct LogsSettings: View {
    @State private var snapshot: DiagnosticExport?
    @State private var exporting = false
    @State private var error: String?
    var body: some View {
        VStack(alignment:.leading,spacing:12) {
            SectionHeader(title:"Logs")
            Button {
                guard !exporting, snapshot == nil else { return }
                exporting = true
                error = nil
                Task {
                    let answer = await Bridge.send("diagnostic-log-home")
                    let home = Diagnostics.gameLogHome(fromEngine: answer)
                    let incomplete = home == nil || answer["flushed"] as? Bool != true
                    do {
                        snapshot = try await Task.detached(priority: .utility) {
                            try Diagnostics.export(gameFolder: home, gameLogMayBeIncomplete: incomplete)
                        }.value
                    } catch {
                        self.error = "Could not export logs. Please try again."
                    }
                    exporting = false
                }
            } label: {Label(exporting ? "Preparing logs…" : "Export log file",systemImage:"square.and.arrow.up")}
            .disabled(exporting || snapshot != nil).accessibilityIdentifier("export-diagnostics")
            if let error {Text(error).foregroundStyle(.red)}
        }.padding(16).card()
        .sheet(item: $snapshot) { export in
            DiagnosticFileExport(snapshot: export) {
                if snapshot === export { snapshot = nil }
            }
        }
    }
}
private struct DiagnosticFileExport: UIViewControllerRepresentable {
    let snapshot: DiagnosticExport
    let finish: () -> Void
    func makeCoordinator() -> Coordinator {Coordinator(snapshot: snapshot, finish: finish)}
    func makeUIViewController(context: Context) -> UIDocumentPickerViewController {
        let picker = UIDocumentPickerViewController(forExporting: [snapshot.url], asCopy: true)
        picker.delegate = context.coordinator
        picker.view.accessibilityIdentifier = "diagnostic-file-export"
        return picker
    }
    func updateUIViewController(_ controller:UIDocumentPickerViewController,context:Context) {}
    final class Coordinator: NSObject, UIDocumentPickerDelegate {
        private let snapshot: DiagnosticExport
        let finish: () -> Void
        init(snapshot: DiagnosticExport, finish: @escaping () -> Void) {
            self.snapshot = snapshot
            self.finish = finish
        }
        func documentPickerWasCancelled(_ controller:UIDocumentPickerViewController) {finish()}
        func documentPicker(_ controller:UIDocumentPickerViewController,didPickDocumentsAt urls:[URL]) {finish()}
    }
}
