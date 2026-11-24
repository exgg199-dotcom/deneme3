import CoreFoundation
import Foundation

/// How the game answers: the context the launcher passed, and the answer as NUL-terminated UTF-8 JSON that
/// lives only for the call.
public typealias HorizonDone = @convention(c) (UnsafeMutableRawPointer?, UnsafePointer<CChar>?) -> Void
/// How the launcher asks: one line of the game's control language (docs/horizon-ctl-commands.md), a context,
/// and where the answer goes. `horizon-ios-launcher` hands this over with `horizon_launcher_present`.
public typealias HorizonSubmit = @convention(c) (UnsafePointer<CChar>?, UnsafeMutableRawPointer?, HorizonDone) -> Void

/// The launcher's line to the game under it (crates/horizon-ios-launcher, crates/horizon-client/src/game/launcher.rs).
/// The game reads the line on its next frame, on this same main thread, so nothing here blocks: the answer
/// arrives through the callback, which resumes the waiting task.
enum Bridge {
    /// Set by `horizon_launcher_present`; nil in a preview, where nothing is under the launcher.
    static var submit: HorizonSubmit?

    /// Send `line` to the game and wait for its JSON answer. A game that cannot answer gives `["error": ...]`.
    @MainActor
    static func send(_ line: String, responseTimeout: Duration? = nil) async -> [String: Any] {
        guard !line.utf8.contains(0) else {
            return ["error": "The launcher request contains a null byte."]
        }
        let verb = String(line.prefix(while: { !$0.isWhitespace }))
        // Status views report state changes without filling the log on every poll.
        let logReplyError = verb != "download-state" && verb != "shader-pack-status"
            && verb != "diagnostic-log-home" && line != "options-get resourcePacks"
        if verb != "launcher-state", logReplyError {
            Diagnostics.record("Launcher request: \(verb)")
        }
        guard let submit else {
            return ["error": "no game is running under the launcher"]
        }
        let reply = Reply(logError: logReplyError)
        let message: String
        let deadline: Duration
        switch verb {
        case "launcher-retry-loader":
            message = "The game did not respond to the loader retry."
            deadline = .seconds(5)
        case "download-state", "download-retry":
            message = "The game did not respond to the download request."
            deadline = .seconds(5)
        case "launcher-state":
            message = "The game did not report its startup status."
            deadline = .seconds(5)
        case "diagnostic-log-home":
            message = "The game did not provide its log directory."
            deadline = .seconds(1)
        case "worlds", "servers":
            message = "The game did not return the requested list. Try again."
            deadline = .seconds(5)
        case "server-ping":
            message = "The game did not report the server’s status. Try again."
            deadline = .seconds(10)
        case "game-files-source":
            message = "The game did not provide a download source. Retry or use a download link."
            deadline = .seconds(5)
        case "launcher-visible":
            message = "The game did not confirm launcher visibility."
            deadline = .seconds(5)
        // Owned launch operations reconcile with the engine before enabling
        // another Play, so a late acknowledgment cannot start a second game.
        case "launcher-start":
            message = "The game did not confirm that it started."
            deadline = .seconds(15)
        case "launcher-commit-start":
            message = "The game did not confirm that it was ready to show."
            deadline = .seconds(15)
        case "launcher-cancel-start":
            message = "The game did not confirm that it stopped. Tap Cancel to retry."
            deadline = .seconds(15)
        case "launcher-session":
            message = "The game did not confirm your account setup."
            deadline = .seconds(15)
        case "connect", "world-create":
            message = "The game did not confirm that it accepted the request."
            deadline = .seconds(15)
        case "launcher-world" where responseTimeout != nil:
            // Only opening a world opts in. Backup and other file operations
            // keep waiting for their worker to finish.
            message = "The game did not confirm that it accepted the request."
            deadline = .seconds(15)
        case "options-get", "shader-pack", "shader-pack-status":
            message = "The game did not return the requested settings. Try again."
            deadline = .seconds(5)
        case "options-set", "shader-pack-set":
            message = "The game did not confirm that the setting changed. Refresh and try again."
            deadline = .seconds(15)
        default:
            return await submitAndWait(line, using: submit, reply: reply)
        }
        let timeout = Task {
            do { try await Task.sleep(for: responseTimeout ?? deadline) }
            catch { return }
            reply.cancel(message: message)
        }
        defer { timeout.cancel() }
        return await withTaskCancellationHandler {
            await submitAndWait(line, using: submit, reply: reply)
        } onCancel: {
            reply.cancel()
        }
    }

    @MainActor
    private static func submitAndWait(_ line: String, using submit: HorizonSubmit, reply: Reply) async -> [String: Any] {
        await withCheckedContinuation { continuation in
            guard reply.wait(using: continuation) else {
                continuation.resume(returning: ["error": "The launcher request was cancelled."])
                return
            }
            // The engine owns this retained reference until its callback,
            // including when the Swift status poll has already been cancelled.
            let context = Unmanaged.passRetained(reply).toOpaque()
            line.withCString { submit($0, context, replied) }
        }
    }

    /// `launcher-world` with its arguments as JSON, so a folder with spaces in its name arrives whole.
    @MainActor
    static func world(_ action: String, folder: String, name: String? = nil,
                      openTimeout: Duration = .seconds(15)) async -> [String: Any] {
        var args: [String: String] = ["action": action, "folder": folder]
        args["name"] = name
        guard let data = try? JSONSerialization.data(withJSONObject: args),
              let json = String(data: data, encoding: .utf8)
        else {
            return ["error": "unreadable world name"]
        }
        return await send("launcher-world \(json)", responseTimeout: action == "open" ? openTimeout : nil)
    }
}

private final class Reply: @unchecked Sendable {
    typealias Continuation = CheckedContinuation<[String: Any], Never>

    private let lock = NSLock()
    private var continuation: Continuation?
    private var finished = false
    let logError: Bool

    init(logError: Bool) {
        self.logError = logError
    }

    func wait(using continuation: Continuation) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        guard !finished else { return false }
        self.continuation = continuation
        return true
    }

    func takeContinuation() -> Continuation? {
        lock.lock()
        defer { lock.unlock() }
        finished = true
        let waiting = continuation
        continuation = nil
        return waiting
    }

    func cancel(message: String = "The launcher request was cancelled.") {
        takeContinuation()?.resume(returning: ["error": message])
    }
}

/// The game calls this once per line it was sent; the context is the retained `Reply`, released here.
private let replied: HorizonDone = { context, text in
    guard let context else { return }
    let reply = Unmanaged<Reply>.fromOpaque(context).takeRetainedValue()
    guard let continuation = reply.takeContinuation() else { return }
    let answer = text
        .flatMap { String(validatingCString: $0) }
        .flatMap { (try? JSONSerialization.jsonObject(with: Data($0.utf8))) as? [String: Any] }
        ?? ["error": "the game's answer was not JSON"]
    if reply.logError, let error = answer.error {
        Diagnostics.record("Launcher error: \(error)")
    }
    continuation.resume(returning: answer)
}

extension Dictionary where Key == String, Value == Any {
    /// The answer's error, when it has one.
    var error: String? { self["error"] as? String }

    /// JSON numbers bridge to NSNumber too. Only a true Boolean acknowledges a command.
    var acknowledged: Bool {
        guard let flag = self["ok"] as? NSNumber, CFGetTypeID(flag) == CFBooleanGetTypeID() else {
            return false
        }
        return flag.boolValue
    }
}
