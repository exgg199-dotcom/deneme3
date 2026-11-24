import AVFoundation
import CryptoKit
import Darwin
import os
import XCTest
import UIKit
@testable import LauncherPreview

final class GameAudioTests: XCTestCase {
    #if compiler(>=6.4)
    func testModernAudioResumptionIgnoresLegacyEventsAndMissingContext() async throws {
        guard #available(iOS 27.0, *) else { throw XCTSkip("Requires iOS 27 audio notifications") }
        let queue = DispatchQueue(label: "horizon.audio.modern.test")
        let notifications = NotificationCenter()
        let transitions = OSAllocatedUnfairLock(initialState: [Bool]())
        let audio = GameAudio(queue: queue, notifications: notifications) { active in
            transitions.withLock { $0.append(active) }
        }
        XCTAssertEqual(GameAudio.ResumptionEvent.system.name, AVAudioSession.resumptionRecommendationNotification)
        XCTAssertEqual(GameAudio.ResumptionEvent.system.interruptionName, AVAudioSession.didBecomeInactiveNotification)
        func drain() async {
            await withCheckedContinuation { continuation in
                queue.async { continuation.resume() }
            }
        }

        audio.start()
        await drain()
        notifications.post(name: AVAudioSession.interruptionNotification, object: nil,
                           userInfo: [AVAudioSessionInterruptionTypeKey: AVAudioSession.InterruptionType.ended.rawValue])
        notifications.post(name: AVAudioSession.resumptionRecommendationNotification, object: nil)
        await drain()
        XCTAssertEqual(transitions.withLock { $0 }, [true])
        audio.stop()
        await drain()
        XCTAssertEqual(transitions.withLock { $0 }, [true, false])
    }
    #endif

    func testFailedAudioActivationRetriesOnceUnlessTheGameHasStopped() async {
        struct ActivationError: Error {}
        for quit in [false, true] {
            let queue = DispatchQueue(label: "horizon.audio.retry.test")
            let transitions = OSAllocatedUnfairLock(initialState: [Bool]())
            let activated = expectation(description: "retried activation")
            activated.isInverted = quit
            let audio = GameAudio(queue: queue, notifications: NotificationCenter(),
                                  resumption: .interruptionEnded) { active in
                let first = transitions.withLock { state in
                    defer { state.append(active) }
                    return state.isEmpty
                }
                if first { throw ActivationError() }
                if active { activated.fulfill() }
            }

            queue.suspend()
            audio.start()
            if quit { audio.stop() }
            queue.resume()
            await fulfillment(of: [activated], timeout: quit ? 0.5 : 2)
            XCTAssertEqual(transitions.withLock { $0 }, quit ? [true, false] : [true, true])
        }
    }

    func testFailedDeactivationRetriesUnlessANewGameStarts() async {
        for startAgain in [false, true] {
            let queue = DispatchQueue(label: "horizon.audio.deactivation-retry.test")
            let transitions = OSAllocatedUnfairLock(initialState: [Bool]())
            let audio = GameAudio(queue: queue, notifications: NotificationCenter(),
                                  resumption: .interruptionEnded) { active in
                let firstStop = transitions.withLock { state in
                    defer { state.append(active) }
                    return !active && !state.contains(false)
                }
                if firstStop { throw CocoaError(.userCancelled) }
            }

            audio.start()
            audio.stop()
            if startAgain { audio.start() }
            await withCheckedContinuation { continuation in
                queue.asyncAfter(deadline: .now() + .milliseconds(500)) { continuation.resume() }
            }
            XCTAssertEqual(transitions.withLock { $0 }, startAgain ? [true, false, true] : [true, false, false])
        }
    }

    func testReturningWithoutAnInterruptionEndRecoversAudio() async {
        struct ActivationError: Error {}
        let queue = DispatchQueue(label: "horizon.audio.interruption-retry.test")
        let notifications = NotificationCenter()
        let transitions = OSAllocatedUnfairLock(initialState: [Bool]())
        let audio = GameAudio(queue: queue, notifications: notifications, resumption: .interruptionEnded) { active in
            let first = transitions.withLock { state in
                defer { state.append(active) }
                return state.isEmpty
            }
            if first { throw ActivationError() }
        }
        func drain() async {
            await withCheckedContinuation { continuation in queue.async { continuation.resume() } }
        }
        audio.start()
        await drain()
        notifications.post(name: AVAudioSession.interruptionNotification, object: nil,
                           userInfo: [AVAudioSessionInterruptionTypeKey: AVAudioSession.InterruptionType.began.rawValue])
        try? await Task.sleep(for: .milliseconds(400))
        await drain()
        XCTAssertEqual(transitions.withLock { $0 }, [true])

        notifications.post(name: UIApplication.didBecomeActiveNotification, object: nil)
        await drain()
        XCTAssertEqual(transitions.withLock { $0 }, [true, true])
        audio.stop()
        await drain()
    }

    func testAudioTransitionsAreSerializedAndQuitIgnoresAnOlderInterruption() async {
        let queue = DispatchQueue(label: "horizon.audio.test")
        let notifications = NotificationCenter()
        let transitions = OSAllocatedUnfairLock(initialState: [Bool]())
        let audio = GameAudio(queue: queue, notifications: notifications, resumption: .interruptionEnded) { active in
            XCTAssertFalse(Thread.isMainThread)
            transitions.withLock { $0.append(active) }
        }
        func endInterruption() {
            notifications.post(
                name: AVAudioSession.interruptionNotification,
                object: nil,
                userInfo: [AVAudioSessionInterruptionTypeKey: AVAudioSession.InterruptionType.ended.rawValue]
            )
        }
        func drain() async {
            await withCheckedContinuation { continuation in
                queue.async { continuation.resume() }
            }
        }

        audio.stop()
        await drain()
        XCTAssertEqual(transitions.withLock { $0 }, [false])

        audio.start()
        await drain()
        endInterruption()
        await drain()
        XCTAssertEqual(transitions.withLock { $0 }, [false, true, true])

        // Capture the old observer's event before the queued Quit removes it.
        queue.suspend()
        audio.stop()
        audio.start()
        endInterruption()
        queue.resume()
        await drain()
        XCTAssertEqual(transitions.withLock { $0 }, [false, true, true, false, true])

        audio.stop()
        await drain()
        endInterruption()
        await drain()
        XCTAssertEqual(transitions.withLock { $0 }, [false, true, true, false, true, false])
    }

    func testSystemDeactivationAndNegativeResumptionCancelPendingRetry() async {
        struct ActivationError: Error {}
        let inactive = Notification.Name("test-audio-inactive")
        let resume = Notification.Name("test-audio-resume")
        let events = GameAudio.ResumptionEvent(
            name: resume,
            recommendation: { _ in .stayInactive },
            interruptionName: inactive,
            isInterruption: { $0.userInfo?["system"] as? Bool == true }
        )
        for (name, system, cancelsRetry) in [(inactive, true, true), (resume, false, true), (inactive, false, false)] {
            let queue = DispatchQueue(label: "horizon.audio.system-notifications.test")
            let notifications = NotificationCenter()
            let transitions = OSAllocatedUnfairLock(initialState: [Bool]())
            let retried = expectation(description: "activation retry")
            retried.isInverted = cancelsRetry
            let audio = GameAudio(queue: queue, notifications: notifications, resumption: events) { active in
                let first = transitions.withLock { state in
                    defer { state.append(active) }
                    return state.isEmpty
                }
                if first { throw ActivationError() }
                if active { retried.fulfill() }
            }
            func drain() async {
                await withCheckedContinuation { continuation in queue.async { continuation.resume() } }
            }
            audio.start()
            await drain()
            notifications.post(name: name, object: nil, userInfo: ["system": system])
            await fulfillment(of: [retried], timeout: cancelsRetry ? 0.5 : 2)
            XCTAssertEqual(transitions.withLock { $0 }, cancelsRetry ? [true] : [true, true])
            audio.stop()
            await drain()
        }
    }
}

@MainActor
private enum DelayedStatusCallback {
    private static var reply: (UnsafeMutableRawPointer?, HorizonDone)?
    static var hasReply: Bool { reply != nil }

    static func store(_ context: UnsafeMutableRawPointer?, _ done: HorizonDone) {
        reply = (context, done)
    }

    static func complete() {
        guard let (context, done) = reply else { return }
        reply = nil
        #"{"phase":"waiting"}"#.withCString { done(context, $0) }
    }
}

@MainActor
final class BridgeInputTests: XCTestCase {
    func testCancelledStatusPollDoesNotWaitForTheEngineCallback() async {
        let previous = Bridge.submit
        defer {
            Bridge.submit = previous
            DelayedStatusCallback.complete()
        }
        Bridge.submit = { _, context, done in
            MainActor.assumeIsolated { DelayedStatusCallback.store(context, done) }
        }
        let finished = expectation(description: "status poll returned before the engine answered")
        let poll = Task {
            let answer = await Bridge.send("launcher-state")
            XCTAssertEqual(answer.error, "The launcher request was cancelled.")
            finished.fulfill()
        }
        while !DelayedStatusCallback.hasReply { await Task.yield() }
        poll.cancel()
        await fulfillment(of: [finished], timeout: 1)
        // The delayed callback still owns its context and must be safe even
        // after the poll has returned.
        DelayedStatusCallback.complete()
        await poll.value
    }

    func testBoundedRequestsTimeOutAndIgnoreLateEngineReplies() async {
        let previous = Bridge.submit
        defer {
            Bridge.submit = previous
            DelayedStatusCallback.complete()
        }
        let cases = [
            ("download-state", "The game did not respond to the download request."),
            ("download-retry", "The game did not respond to the download request."),
            ("launcher-state", "The game did not report its startup status."),
            ("diagnostic-log-home", "The game did not provide its log directory."),
            ("worlds", "The game did not return the requested list. Try again."),
            ("servers", "The game did not return the requested list. Try again."),
            ("server-ping example.invalid", "The game did not report the server’s status. Try again."),
            ("game-files-source", "The game did not provide a download source. Retry or use a download link."),
            ("launcher-visible true", "The game did not confirm launcher visibility."),
            ("launcher-start", "The game did not confirm that it started."),
            ("launcher-start test-owner", "The game did not confirm that it started."),
            ("launcher-commit-start test-owner", "The game did not confirm that it was ready to show."),
            ("launcher-cancel-start test-owner", "The game did not confirm that it stopped. Tap Cancel to retry."),
            ("launcher-session off", "The game did not confirm your account setup."),
            ("connect example.invalid", "The game did not confirm that it accepted the request."),
            ("world-create {}", "The game did not confirm that it accepted the request."),
            ("options-get resourcePacks", "The game did not return the requested settings. Try again."),
            ("shader-pack list", "The game did not return the requested settings. Try again."),
            ("options-set resourcePacks []", "The game did not confirm that the setting changed. Refresh and try again."),
            ("shader-pack-set {}", "The game did not confirm that the setting changed. Refresh and try again."),
        ]
        for (command, message) in cases {
            Bridge.submit = { _, context, done in
                MainActor.assumeIsolated { DelayedStatusCallback.store(context, done) }
            }
            let finished = expectation(description: "unanswered \(command) returned")
            let request = Task {
                let reply = await Bridge.send(command, responseTimeout: .milliseconds(20))
                XCTAssertEqual(reply.error, message)
                finished.fulfill()
            }
            await fulfillment(of: [finished], timeout: 1)
            XCTAssertTrue(DelayedStatusCallback.hasReply)
            DelayedStatusCallback.complete()
            await request.value
        }

        Bridge.submit = { _, context, done in
            #"{"phase":"installing"}"#.withCString { done(context, $0) }
        }
        let next = await Bridge.send("download-state")
        XCTAssertEqual(next["phase"] as? String, "installing")
    }

    func testOpeningAWorldWithSpacesTimesOutAndReleasesItsCaller() async {
        let previous = Bridge.submit
        defer {
            Bridge.submit = previous
            DelayedStatusCallback.complete()
        }
        Bridge.submit = { line, context, done in
            MainActor.assumeIsolated {
                let command = line.map { String(cString: $0) } ?? ""
                let json = Data(command.dropFirst("launcher-world ".count).utf8)
                let arguments = (try? JSONSerialization.jsonObject(with: json)) as? [String: String]
                XCTAssertEqual(arguments, ["action": "open", "folder": "World With Spaces"])
                DelayedStatusCallback.store(context, done)
            }
        }
        let returned = expectation(description: "world opening returned before the engine answered")
        let request = Task {
            let reply = await Bridge.world("open", folder: "World With Spaces", openTimeout: .milliseconds(20))
            XCTAssertEqual(reply.error, "The game did not confirm that it accepted the request.")
            returned.fulfill()
        }
        await fulfillment(of: [returned], timeout: 1)
        XCTAssertTrue(DelayedStatusCallback.hasReply)
        DelayedStatusCallback.complete()
        await request.value
    }

    func testCommandWithNullByteIsRefusedBeforeSubmission() async {
        let previous = Bridge.submit
        defer { Bridge.submit = previous }
        Bridge.submit = { _, context, done in
            "{\"ok\":true}".withCString { done(context, $0) }
        }
        let reply = await Bridge.send("launcher-start\0ignored")
        XCTAssertNotNil(reply.error)
        XCTAssertNil(reply["ok"])
    }

    func testReplyWithInvalidUTF8IsRefusedEvenWhenLossyDecodingWouldReadAsJSON() async {
        let previous = Bridge.submit
        defer { Bridge.submit = previous }
        Bridge.submit = { _, context, done in
            var bytes = Array("{\"ok\":true,\"message\":\"x\"}".utf8CString)
            bytes[bytes.firstIndex(of: 120)!] = -1
            bytes.withUnsafeBufferPointer { done(context, $0.baseAddress) }
        }
        let reply = await Bridge.send("launcher-state")
        XCTAssertNotNil(reply.error)
        XCTAssertNil(reply["ok"])
    }
}

@MainActor
final class LauncherPresentationTests: XCTestCase {
    func testPresentationStackIncludesAnOverlayCoveredByChildScreens() {
        final class Controller: UIViewController {
            var presented: UIViewController?
            override var presentedViewController: UIViewController? { presented }
        }
        let root = Controller()
        let launcher = Controller()
        let editor = Controller()
        let sheet = UIViewController()
        root.presented = launcher
        launcher.presented = editor
        editor.presented = sheet

        let stack = presentationStack(from: root)
        XCTAssertEqual(stack.map(ObjectIdentifier.init),
                       [root, launcher, editor, sheet].map(ObjectIdentifier.init))
        XCTAssertTrue(stack.contains { $0 === launcher })
        XCTAssertTrue(stack.last === sheet)
        XCTAssertTrue(presentationStack(from: nil).isEmpty)
    }

    func testPresentationRetriesAfterWindowDetachesOrStartsTheGameOnExhaustion() async {
        let original = UIViewController()
        let replacement = UIViewController()
        for recover in [true, false] {
            var current: UIViewController? = original
            var events: [String] = []
            let result = await prepareLauncherPresentation(
                attempts: 2,
                presenter: { current },
                setCovered: { visible in
                    events.append("covered \(visible)")
                    if current === original { current = nil }
                    return true
                },
                pause: {
                    events.append("retry")
                    if recover { current = replacement }
                },
                startGame: { events.append("start") }
            )
            if recover {
                XCTAssertTrue(result === replacement)
                XCTAssertEqual(events, ["covered true", "retry", "covered true"])
            } else {
                XCTAssertNil(result)
                XCTAssertEqual(events, ["covered true", "retry", "start", "covered false"])
            }
        }
    }

    func testRejectedCoverDoesNotPresentAndReversesTheRequestAfterFallback() async {
        let root = UIViewController()
        var events: [String] = []
        let result = await prepareLauncherPresentation(
            attempts: 2,
            presenter: { root },
            setCovered: { visible in
                events.append("covered \(visible)")
                return !visible
            },
            pause: { events.append("retry") },
            startGame: { events.append("start") }
        )
        XCTAssertNil(result)
        XCTAssertEqual(events, ["covered true", "start", "covered false"])
    }
}

@MainActor
final class ChangelogTests: XCTestCase {
    func testInvalidRowsPreserveCachedChangesAndValidEmptyResultsReplaceThem() async throws {
        let cache = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: cache) }
        let previous = Data(#"[{"sha":"old","commit":{"message":"Previous change"}}]"#.utf8)
        try previous.write(to: cache)
        var reply = Data(#"[{"unrecognized":true}]"#.utf8)
        let log = Changelog(fetch: { request in
            let response = HTTPURLResponse(url: request.url!, statusCode: 200, httpVersion: nil, headerFields: nil)!
            return (reply, response)
        }, cache: cache)

        await log.load()
        XCTAssertNotNil(log.failed)
        XCTAssertEqual(log.changes.map(\.title), ["Previous change"])
        XCTAssertEqual(try Data(contentsOf: cache), previous)

        reply = Data(#"[{"sha":"merge","commit":{"message":"Merge changes"}}]"#.utf8)
        await log.load()
        XCTAssertNil(log.failed)
        XCTAssertTrue(log.changes.isEmpty)
        XCTAssertEqual(try Data(contentsOf: cache), reply)

        reply = Data(#"[{"sha":"new","commit":{"message":"New change"}},{"sha":"new","commit":{"message":"Duplicate"}}]"#.utf8)
        await log.load()
        XCTAssertEqual(log.changes.map(\.title), ["New change"])
    }
}

@MainActor
final class GameLinkRefreshTests: XCTestCase {
    func testUnacknowledgedJoinReconcilesWithoutCommittingPresentation() async {
        for response: [String: Any] in [[:], ["ok": false], ["ok": 1]] {
            var commits = 0
            var cancels = 0
            var presentations = 0
            let game = GameLink(stopAudio: {}) { command in
                if command.hasPrefix("launcher-start ") { return ["ok": true, "phase": "title"] }
                if command.hasPrefix("connect ") { return response }
                if command.hasPrefix("launcher-commit-start ") {
                    commits += 1
                    return ["ok": true, "status": "committed"]
                }
                if command.hasPrefix("launcher-cancel-start ") {
                    cancels += 1
                    return ["ok": true, "status": "stopped"]
                }
                return ["phase": "title"]
            }
            game.prepareSession = {}
            game.uncover = { presentations += 1 }
            await game.refresh()
            await game.join("fixture.invalid extra")
            XCTAssertTrue(game.problem?.hasPrefix("A server address") == true)
            await game.join("\nfixture.invalid ")
            XCTAssertEqual(commits, 0)
            XCTAssertEqual(cancels, 1)
            XCTAssertEqual(presentations, 0)
            XCTAssertNil(game.launching)
            XCTAssertEqual(game.problem, "Horizon could not confirm that the game accepted the request.")
        }
    }

    func testStartupStatusRecoversOnTheNextValidReply() async {
        var status: [String: Any] = ["error": "The game did not report its startup status."]
        let game = GameLink(stopAudio: {}) { _ in status }
        await game.refresh()
        XCTAssertEqual(game.phase, .unknown)
        XCTAssertEqual(game.startupStatusError, "The game did not report its startup status.")

        status = ["phase": "title", "version": "26.3"]
        await game.refresh()
        XCTAssertEqual(game.phase, .title)
        XCTAssertNil(game.startupStatusError)
    }

    func testAnAlreadyCancelledRefreshDoesNotRequestState() async {
        var requests = 0
        let game = GameLink { _ in requests += 1; return ["phase": "title"] }
        let refresh = Task { await game.refresh() }
        refresh.cancel()
        await refresh.value
        XCTAssertEqual(requests, 0)
        XCTAssertEqual(game.phase, .unknown)
    }

    func testAnOlderRefreshCannotReplaceNewerStateOrVersion() async {
        let firstEntered = expectation(description: "first refresh")
        let secondEntered = expectation(description: "second refresh")
        var replies: [CheckedContinuation<[String: Any], Never>] = []
        let game = GameLink { command in
            XCTAssertEqual(command, "launcher-state")
            return await withCheckedContinuation { reply in
                replies.append(reply)
                if replies.count == 1 { firstEntered.fulfill() }
                else { secondEntered.fulfill() }
            }
        }
        let first = Task { await game.refresh() }
        await fulfillment(of: [firstEntered], timeout: 2)
        let second = Task { await game.refresh() }
        await fulfillment(of: [secondEntered], timeout: 2)
        replies[1].resume(returning: ["phase": "title", "version": "current"])
        await second.value
        XCTAssertEqual(game.phase, .title)
        replies[0].resume(returning: ["phase": "waiting", "version": "stale"])
        await first.value
        XCTAssertEqual(game.phase, .title)
        XCTAssertEqual(game.version, "current")
    }

    func testIdleLauncherOffersRepairBeforeStartingTheGame() async {
        var reply: [String: Any] = ["phase": "waiting", "game_files_present": true,
                                     "game_files_problem": "facts.json is empty"]
        let game = GameLink(stopAudio: {}) { command in
            XCTAssertEqual(command, "launcher-state")
            return reply
        }
        game.prepareSession = { XCTFail("A damaged installation must open setup before launching") }
        await game.refresh()
        XCTAssertEqual(game.phase, .waiting)
        XCTAssertTrue(game.needsGameFiles)
        XCTAssertTrue(game.repairingGameFiles)
        XCTAssertEqual(game.gameFilesProblem, "facts.json is empty")
        await game.play()
        XCTAssertTrue(game.requestingGameFiles)
        XCTAssertNil(game.launching)

        reply = ["phase": "waiting", "game_files_present": true]
        await game.refresh()
        XCTAssertFalse(game.needsGameFiles)
        XCTAssertFalse(game.repairingGameFiles)
        XCTAssertNil(game.gameFilesProblem)
        XCTAssertFalse(game.requestingGameFiles)
    }

    func testCancellingLoadKeepsKnownGameFilesUntilTheEngineChecksAgain() async {
        let restore = temporarilyRemoveResourceMarker()
        defer { restore() }
        var state: [String: Any] = ["phase": "waiting", "game_files_present": true]
        var startReply: CheckedContinuation<[String: Any], Never>?
        let submitted = expectation(description: "start submitted")
        let game = GameLink(stopAudio: {}) { command in
            if command.hasPrefix("launcher-start ") {
                return await withCheckedContinuation {
                    startReply = $0
                    submitted.fulfill()
                }
            }
            if command.hasPrefix("launcher-cancel-start ") { return ["ok": true, "status": "stopped"] }
            return state
        }
        game.prepareSession = {}
        await game.refresh()
        XCTAssertFalse(game.needsGameFiles)
        let play = Task { await game.play() }
        await fulfillment(of: [submitted], timeout: 2)
        state = ["phase": "loading", "progress": 0.5]
        await game.refresh()
        game.cancelLaunch()
        startReply?.resume(returning: ["ok": true, "phase": "loading"])
        await play.value
        XCTAssertEqual(game.phase, .waiting)
        XCTAssertFalse(game.needsGameFiles, "Cancelling Play must not start an unnecessary download")
        state = ["phase": "waiting", "game_files_present": false]
        await game.refresh()
        XCTAssertTrue(game.needsGameFiles)
    }

    func testQueuedJoinOffersSetupAndResumesOnceAfterRecovery() async {
        var phase = "unknown"
        var commands: [String] = []
        let game = GameLink { command in
            commands.append(command)
            if command == "launcher-state" { return ["phase": phase, "reason": "Files are damaged"] }
            if command.hasPrefix("launcher-start ") { return ["ok": true, "phase": "loading"] }
            if command.hasPrefix("launcher-commit-start ") { return ["ok": true, "status": "committed"] }
            return ["ok": true]
        }
        game.prepareSession = {}
        var presentations = 0
        game.uncover = { presentations += 1 }
        await game.join("example.invalid")
        phase = "needs_files"
        await game.refresh()
        XCTAssertTrue(game.requestingGameFiles)
        XCTAssertFalse(commands.contains("connect example.invalid"))
        game.requestingGameFiles = false
        await game.refresh()
        XCTAssertFalse(game.requestingGameFiles, "The same failure must not reopen dismissed setup")

        phase = "title"
        await game.refresh()
        await game.refresh()
        XCTAssertEqual(commands.filter { $0 == "connect example.invalid" }.count, 1)
        XCTAssertEqual(presentations, 1)
        XCTAssertNil(game.launching)
    }

    func testGameFilesSetupClosesWhenALaterPollConfirmsRecovery() async {
        var status: [String: Any] = ["phase": "needs_files", "reason": "Damaged tables"]
        let game = GameLink(stopAudio: {}) { _ in status }
        await game.refresh()
        game.requestingGameFiles = true

        await game.refresh()
        XCTAssertTrue(game.requestingGameFiles)
        // Stale phase.
        status = ["error": "Poll failed", "phase": "title"]
        await game.refresh()
        XCTAssertEqual(game.startupStatusError, "Poll failed")
        XCTAssertTrue(game.requestingGameFiles)

        status = ["phase": "waiting", "game_files_present": true]
        await game.refresh()
        XCTAssertFalse(game.requestingGameFiles)
        XCTAssertFalse(game.needsGameFiles)
    }

    func testEarlierStartReplyCannotHideRecoveryAlreadyReportedByRefresh() async {
        let submitted = expectation(description: "start submitted")
        var reply: CheckedContinuation<[String: Any], Never>?
        let game = GameLink { command in
            if command.hasPrefix("launcher-start ") {
                return await withCheckedContinuation {
                    reply = $0
                    submitted.fulfill()
                }
            }
            return ["phase": "needs_files", "reason": "Files are damaged"]
        }
        game.prepareSession = {}
        let join = Task { await game.join("example.invalid") }
        await fulfillment(of: [submitted], timeout: 2)
        await game.refresh()
        reply?.resume(returning: ["ok": true, "phase": "loading"])
        await join.value

        XCTAssertEqual(game.phase, .needsFiles("Files are damaged"))
        XCTAssertEqual(game.gameFilesProblem, "Files are damaged")
        XCTAssertTrue(game.requestingGameFiles)
    }

    func testPlayDoesNotPrepareOrStartTheEngineDuringCacheCleanup() async {
        var commands: [String] = []
        let game = GameLink(stopAudio: {}) { command in
            commands.append(command)
            return ["phase": "waiting", "game_files_present": false, "cache_cleanup_in_progress": true]
        }
        var preparations = 0
        game.prepareSession = { preparations += 1 }
        await game.refresh()
        await game.play()
        await game.join("example.invalid")
        XCTAssertTrue(game.cleaningUpCache)
        XCTAssertFalse(game.needsGameFiles || game.requestingGameFiles)
        XCTAssertNil(game.launching)
        XCTAssertEqual(preparations, 0)
        XCTAssertEqual(commands, ["launcher-state"])
    }

    func testCleanupStatusDoesNotSupersedeAQueuedStartReply() async {
        let submitted = expectation(description: "start submitted")
        var startReply: CheckedContinuation<[String: Any], Never>?
        let game = GameLink(stopAudio: {}) { command in
            if command.hasPrefix("launcher-start ") {
                return await withCheckedContinuation {
                    startReply = $0
                    submitted.fulfill()
                }
            }
            return ["phase": "waiting", "cache_cleanup_in_progress": true]
        }
        game.prepareSession = {}
        let join = Task { await game.join("example.invalid") }
        await fulfillment(of: [submitted], timeout: 2)

        await game.refresh()
        XCTAssertEqual(game.phase, .waiting)
        startReply?.resume(returning: ["ok": true, "phase": "loading"])
        await join.value
        XCTAssertEqual(game.phase, .loading(0))
    }

    func testEarlierStartReplyCannotSendTheLauncherBackFromTheTitle() async {
        let submitted = expectation(description: "start submitted")
        var startReply: CheckedContinuation<[String: Any], Never>?
        var connects = 0
        let game = GameLink { command in
            if command.hasPrefix("launcher-start ") {
                return await withCheckedContinuation {
                    startReply = $0
                    submitted.fulfill()
                }
            }
            if command == "launcher-state" { return ["phase": "title"] }
            if command.hasPrefix("launcher-commit-start ") { return ["ok": true, "status": "committed"] }
            if command == "connect example.invalid" { connects += 1 }
            return ["ok": true]
        }
        game.prepareSession = {}
        let join = Task { await game.join("example.invalid") }
        await fulfillment(of: [submitted], timeout: 2)
        await game.refresh()
        startReply?.resume(returning: ["ok": true, "phase": "loading"])
        await join.value

        XCTAssertEqual(game.phase, .title)
        XCTAssertEqual(connects, 1)
        XCTAssertNil(game.launching)
    }

    func testStartReplyDoesNotDiscardANewerRefreshStillInFlight() async {
        let submitted = expectation(description: "start submitted")
        let refreshing = expectation(description: "state requested")
        var startReply: CheckedContinuation<[String: Any], Never>?
        var stateReply: CheckedContinuation<[String: Any], Never>?
        let game = GameLink { command in
            if command.hasPrefix("launcher-start ") {
                return await withCheckedContinuation {
                    startReply = $0
                    submitted.fulfill()
                }
            }
            return await withCheckedContinuation {
                stateReply = $0
                refreshing.fulfill()
            }
        }
        game.prepareSession = {}
        let join = Task { await game.join("example.invalid") }
        await fulfillment(of: [submitted], timeout: 2)
        let refresh = Task { await game.refresh() }
        await fulfillment(of: [refreshing], timeout: 2)
        startReply?.resume(returning: ["ok": true, "phase": "loading"])
        await join.value
        XCTAssertEqual(game.phase, .loading(0))

        stateReply?.resume(returning: ["phase": "loading", "progress": 0.6])
        await refresh.value
        XCTAssertEqual(game.phase, .loading(0.6))
    }

    func testStartDiscardsAStateReadMadeDuringAccountPreparation() async {
        let preparing = expectation(description: "account preparation started")
        let refreshing = expectation(description: "state requested before start")
        var prepare: CheckedContinuation<Void, Never>?
        var stateReply: CheckedContinuation<[String: Any], Never>?
        let game = GameLink { command in
            if command.hasPrefix("launcher-start ") { return ["ok": true, "phase": "loading"] }
            return await withCheckedContinuation {
                stateReply = $0
                refreshing.fulfill()
            }
        }
        game.prepareSession = {
            await withCheckedContinuation {
                prepare = $0
                preparing.fulfill()
            }
        }
        let join = Task { await game.join("example.invalid") }
        await fulfillment(of: [preparing], timeout: 2)
        let refresh = Task { await game.refresh() }
        await fulfillment(of: [refreshing], timeout: 2)
        prepare?.resume()
        await join.value

        stateReply?.resume(returning: ["phase": "title"])
        await refresh.value
        XCTAssertEqual(game.phase, .loading(0))
    }

    func testACancelledRefreshDoesNotPublishItsReply() async {
        let entered = expectation(description: "held refresh")
        var hold = false
        var reply: CheckedContinuation<[String: Any], Never>?
        let game = GameLink { _ in
            if !hold { return ["phase": "title", "version": "current"] }
            return await withCheckedContinuation { pending in
                reply = pending
                entered.fulfill()
            }
        }
        await game.refresh()
        hold = true
        let refresh = Task { await game.refresh() }
        await fulfillment(of: [entered], timeout: 2)
        refresh.cancel()
        reply?.resume(returning: ["phase": "waiting", "version": "cancelled"])
        await refresh.value
        XCTAssertEqual(game.phase, .title)
        XCTAssertEqual(game.version, "current")
    }
}

final class FormattingTests: XCTestCase {
    func testGalleryOnlyListsFilesAndDoesNotDeleteAReplacementFolder() throws {
        let files = FileManager.default
        let directory = files.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try files.createDirectory(at: directory, withIntermediateDirectories: false)
        defer { try? files.removeItem(at: directory) }

        let original = directory.appendingPathComponent("Screenshot.PNG")
        let folder = directory.appendingPathComponent("Folder.png")
        let link = directory.appendingPathComponent("Link.png")
        try Data("fixture".utf8).write(to: original)
        try files.createDirectory(at: folder, withIntermediateDirectories: false)
        try files.createSymbolicLink(at: link, withDestinationURL: original)

        let shots = Shot.all(in: directory)
        XCTAssertEqual(shots.map { $0.url.lastPathComponent }, ["Screenshot.PNG"])
        let shot = try XCTUnwrap(shots.first { $0.url.lastPathComponent == "Screenshot.PNG" })
        try shot.remove()
        XCTAssertTrue(Shot.all(in: directory).isEmpty)
        try shot.remove()

        try files.createDirectory(at: original, withIntermediateDirectories: false)
        let child = original.appendingPathComponent("keep.txt")
        try Data("keep".utf8).write(to: child)
        XCTAssertThrowsError(try shot.remove())
        XCTAssertEqual(try Data(contentsOf: child), Data("keep".utf8))
    }

    func testGalleryDetectsAReplacementAtTheSamePathAndPreservesItOnDelete() throws {
        let files = FileManager.default
        let folder = files.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try files.createDirectory(at: folder, withIntermediateDirectories: false)
        defer { try? files.removeItem(at: folder) }
        let path = folder.appendingPathComponent("Screenshot.png")
        try Data("first".utf8).write(to: path)
        let original = try XCTUnwrap(Shot.all(in: folder).first)

        try Data("other".utf8).write(to: path, options: .atomic)
        try files.setAttributes([.modificationDate: original.date], ofItemAtPath: path.path)
        let replacement = try XCTUnwrap(Shot.all(in: folder).first)
        XCTAssertEqual(original.id, replacement.id)
        XCTAssertNotEqual(original, replacement)
        XCTAssertThrowsError(try original.remove())
        XCTAssertEqual(try Data(contentsOf: path), Data("other".utf8))
        try replacement.remove()
        XCTAssertTrue(Shot.all(in: folder).isEmpty)
    }

    func testScreenshotShareKeepsItsOwnBytesAndRejectsAStaleSelection() throws {
        let manager = FileManager.default
        let folder = manager.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try manager.createDirectory(at: folder, withIntermediateDirectories: false)
        defer { try? manager.removeItem(at: folder) }
        let path = folder.appendingPathComponent("Screenshot.png")
        let original = Data(repeating: 42, count: (1 << 20) + 5)
        try original.write(to: path)
        let shot = try XCTUnwrap(Shot.all(in: folder).first)
        var exported: SharedScreenshot? = try shot.copyForSharing()
        let exportedURL = try XCTUnwrap(exported?.url)
        XCTAssertEqual(exportedURL.lastPathComponent, path.lastPathComponent)
        XCTAssertNotEqual(exportedURL, path)

        try Data("replacement".utf8).write(to: path, options: .atomic)
        XCTAssertThrowsError(try shot.copyForSharing())
        XCTAssertEqual(try Data(contentsOf: exportedURL), original)
        withExtendedLifetime(exported) {}
        exported = nil
        XCTAssertFalse(manager.fileExists(atPath: exportedURL.deletingLastPathComponent().path))

        try manager.removeItem(at: path)
        let other = folder.appendingPathComponent("other.png")
        try original.write(to: other)
        try manager.createSymbolicLink(at: path, withDestinationURL: other)
        XCTAssertThrowsError(try shot.copyForSharing())
    }

    @MainActor
    func testGalleryThumbnailCacheReloadsWhenAnImageIsReplaced() async throws {
        let files = FileManager.default
        let folder = files.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try files.createDirectory(at: folder, withIntermediateDirectories: false)
        defer { try? files.removeItem(at: folder) }
        let path = folder.appendingPathComponent("Screenshot.png")
        // Oversized PNG.
        let rect = CGRect(x: 0, y: 0, width: 5000, height: 12)
        let renderer = UIGraphicsImageRenderer(size: rect.size)
        func writeImage(_ color: UIColor) throws {
            try renderer.pngData { context in
                context.cgContext.setFillColor(color.cgColor)
                context.cgContext.fill(rect)
            }.write(to: path, options: .atomic)
        }

        let thumbnails = Thumbnails()
        func load(_ shot: Shot) async {
            thumbnails.load(shot, side: 12)
            let deadline = ContinuousClock.now + .seconds(5)
            while thumbnails.images[shot] == nil, ContinuousClock.now < deadline {
                try? await Task.sleep(for: .milliseconds(10))
            }
            XCTAssertNotNil(thumbnails.images[shot])
        }

        try writeImage(.red)
        let first = try XCTUnwrap(Shot.all(in: folder).first)
        thumbnails.retain([first])
        await load(first)
        XCTAssertEqual(first.fullImage()?.cgImage?.width, 4096)

        try writeImage(.blue)
        let replacement = try XCTUnwrap(Shot.all(in: folder).first)
        XCTAssertNil(first.decode { _ in true })
        XCTAssertNil(first.fullImage())
        thumbnails.retain([replacement])
        XCTAssertNil(thumbnails.images[first])
        await load(replacement)
        XCTAssertEqual(Set(thumbnails.images.keys), Set([replacement]))
    }

    @MainActor
    func testGalleryEvictsOldThumbnailsAndReloadsThemOnReturn() async throws {
        let files = FileManager.default
        let folder = files.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try files.createDirectory(at: folder, withIntermediateDirectories: false)
        defer { try? files.removeItem(at: folder) }
        let renderer = UIGraphicsImageRenderer(size: CGSize(width: 100, height: 100))
        for name in ["A.png", "B.png", "C.png"] {
            try renderer.pngData { context in
                context.cgContext.setFillColor(UIColor.red.cgColor)
                context.cgContext.fill(CGRect(x: 0, y: 0, width: 100, height: 100))
            }.write(to: folder.appendingPathComponent(name))
        }
        let shots = Shot.all(in: folder)
        XCTAssertEqual(shots.count, 3)
        guard shots.count == 3 else { return }
        let thumbnails = Thumbnails(capacity: 2)
        thumbnails.retain(shots)

        func load(_ shot: Shot) async {
            thumbnails.load(shot, side: 12)
            let deadline = ContinuousClock.now + .seconds(5)
            while thumbnails.images[shot] == nil, ContinuousClock.now < deadline {
                try? await Task.sleep(for: .milliseconds(10))
            }
            XCTAssertNotNil(thumbnails.images[shot])
        }

        for shot in shots { await load(shot) }
        XCTAssertEqual(Set(thumbnails.images.keys), Set(shots.dropFirst()))
        await load(shots[0])
        XCTAssertEqual(Set(thumbnails.images.keys), Set([shots[0], shots[2]]))

        for shot in shots {
            thumbnails.show(shot, side: 12)
            await load(shot)
        }
        XCTAssertEqual(Set(thumbnails.images.keys), Set(shots))
        thumbnails.hide(shots[0])
        XCTAssertEqual(Set(thumbnails.images.keys), Set(shots.dropFirst()))
    }

    @MainActor
    func testGalleryRefreshDiscardsAScanInvalidatedByDeletion() async throws {
        let files = FileManager.default
        let folder = files.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try files.createDirectory(at: folder, withIntermediateDirectories: false)
        defer { try? files.removeItem(at: folder) }
        try Data("fixture".utf8).write(to: folder.appendingPathComponent("Old.png"))
        let old = Shot.all(in: folder)
        let started = expectation(description: "stale scan started")
        var reply: CheckedContinuation<[Shot], Never>?
        var attempts = 0
        let refresh = GalleryRefresh(scan: {
            attempts += 1
            if attempts == 1 {
                return await withCheckedContinuation {
                    reply = $0
                    started.fulfill()
                }
            }
            return Shot.all(in: folder)
        })
        var published: [[Shot]] = []
        let first = Task { await refresh.reload { published.append($0) } }
        await fulfillment(of: [started], timeout: 2)
        refresh.invalidate()
        try old.first?.remove()
        let next = Task { await refresh.reload { published.append($0) } }
        reply?.resume(returning: old)
        await first.value
        await next.value
        XCTAssertEqual(published, [[]])
        XCTAssertGreaterThanOrEqual(attempts, 2)
    }

    func testGameFileDetectionRequiresARegularManifestFile() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let marker = root.appendingPathComponent("horizon-resources.json")
        try FileManager.default.createDirectory(at: marker, withIntermediateDirectories: true)
        XCTAssertFalse(GameFolder.containsGameFiles(at: root))
        try FileManager.default.removeItem(at: marker)
        try Data("{}".utf8).write(to: marker)
        XCTAssertTrue(GameFolder.containsGameFiles(at: root))
        try FileManager.default.removeItem(at: marker)
        let target = root.appendingPathComponent("actual-manifest.json")
        try Data("{}".utf8).write(to: target)
        try FileManager.default.createSymbolicLink(at: marker, withDestinationURL: target)
        XCTAssertTrue(GameFolder.containsGameFiles(at: root))
        try FileManager.default.removeItem(at: target)
        XCTAssertFalse(GameFolder.containsGameFiles(at: root))
        try FileManager.default.removeItem(at: marker)
        try FileManager.default.createSymbolicLink(at: marker, withDestinationURL: root)
        XCTAssertFalse(GameFolder.containsGameFiles(at: root))
    }

    func testRedactionKeepsOrdinaryTextWhileMaskingEachIdentifierFormat() {
        for identifier in [
            "HTTPS://example.invalid/private",
            "eYjFixtureValue",
            String(repeating: "x", count: 129),
            "fixture@example.invalid",
            "203.0.113.4:25565",
            "01234567-89ab-cdef-0123-456789abcdef",
            "/Users/fixture/Documents",
        ] {
            XCTAssertEqual(Diagnostics.redact("Shader diagnostic \(identifier)"),
                           "Shader diagnostic [redacted]")
        }
        XCTAssertEqual(Diagnostics.redact("Shader diagnostic completed"), "Shader diagnostic completed")
    }

    func testDiagnosticsMaskIPv6EndpointsAndKeepTimestamps() {
        let input = """
        [12:00:01] [Server thread/INFO]: Player[/[2001:db8::1234]:25565] logged in
        2026-11-17T20:00:01Z network changed to fe80::42%en0, fallback [::ffff:192.0.2.1]:443
        [12:00:02] [Render thread/WARN]: Connecting to 2001:0DB8:0:0:0:0:0:77.
        Local test endpoint ::1 and wildcard ::.
        """
        let expected = """
        [12:00:01] [Server thread/INFO]: Player[/[redacted]] logged in
        2026-11-17T20:00:01Z network changed to [redacted], fallback [redacted]
        [12:00:02] [Render thread/WARN]: Connecting to [redacted].
        Local test endpoint [redacted] and wildcard [redacted].
        """
        XCTAssertEqual(Diagnostics.redactExport(input), expected)
    }

    func testWhitespaceAndTimestampLikeTextDoNotExposeTokenValues() {
        XCTAssertEqual(Diagnostics.redact("Token\t=opaque-fixture"), "[redacted sensitive log message]")
        XCTAssertEqual(Diagnostics.redact("unexpected token near main()"), "unexpected token near main()")
        let text = "[12:00:00] [Render thread/INFO]: 'token'\t :\n[12:00:00] opaque-fixture\n[12:00:01] [Render thread/INFO]: shader ready"
        let safe = Diagnostics.redactExport(text)
        XCTAssertFalse(safe.contains("opaque-fixture"))
        XCTAssertTrue(safe.contains("shader ready"))
    }

    func testXboxAndCamelCaseOAuthFieldsRedactShortValues() {
        for message in [
            #"{"RpsTicket":"d=opaque-fixture"}"#,
            #"{"UserTokens":["opaque-fixture"]}"#,
            #"{"uhs":"opaque-fixture"}"#,
            #"{"userCode":"opaque-fixture"}"#,
            "deviceCode = opaque-fixture",
            "identity_token: opaque-fixture",
        ] {
            XCTAssertEqual(Diagnostics.redact(message), "[redacted sensitive log message]", message)
        }
        XCTAssertEqual(Diagnostics.redact("shader tokenCount: 8"), "shader tokenCount: 8")
        let log = "[12:00:00] [Render thread/INFO]: UserTokens:\nopaque-fixture\n"
            + "[12:00:01] [Render thread/WARN]: shader compile failed\n"
        let safe = Diagnostics.redactExport(log)
        XCTAssertFalse(safe.contains("opaque-fixture"))
        XCTAssertTrue(safe.contains("shader compile failed"))
    }

    func testSensitiveContinuationsDoNotEndAtAnArbitraryBracket() {
        let text = "[12:00:00] [Render thread/INFO]: access_token:\n[opaque-fixture-value]\n[12:00:01] [Render thread/INFO]: shader ready\n"
        let safe = Diagnostics.redactExport(text)
        XCTAssertFalse(safe.contains("opaque-fixture-value"))
        XCTAssertTrue(safe.contains("shader ready"))
    }

    func testSensitiveFieldsAlsoRedactEarlierLinesOfTheirLogEntry() {
        let text = """
        [12:00:00] [Render thread/INFO]: Previous shader diagnostic
        [12:00:01] [Render thread/INFO]: Account response
        {"code":"opaque-fixture-first",
        "access_token":"opaque-fixture-label",
        "value":"opaque-fixture-last"}
        [12:00:02] [Render thread/INFO]: Next shader diagnostic
        """
        let safe = Diagnostics.redactExport(text)
        XCTAssertFalse(safe.contains("opaque-fixture"))
        XCTAssertTrue(safe.contains("Previous shader diagnostic"))
        XCTAssertTrue(safe.contains("Next shader diagnostic"))
    }

    func testBoundedReadDropsTheWholeLeadingPartialMessage() throws {
        let file = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer {try? FileManager.default.removeItem(at: file)}
        let text = "[12:00:00] [Render thread/INFO]: access_token:\n" + String(repeating: "A", count: Diagnostics.limit) + "\nopaque-fixture-value\n[12:00:01] [Render thread/INFO]: shader ready\n"
        try Data(text.utf8).write(to: file)
        let safe = Diagnostics.redactExport(try Diagnostics.read(file))
        XCTAssertFalse(safe.contains("opaque-fixture-value"))
        XCTAssertTrue(safe.contains("shader ready"))
    }

    func testBoundedReadKeepsACompleteEntryAtTheExactTailBoundary() throws {
        let file = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: file) }
        let preceding = "[12:00:00] [Render/INFO]: previous entry\n"
        let first = "[12:00:01] [Render/INFO]: first complete entry\n"
        let text = preceding + first + String(repeating: "a", count: Diagnostics.limit - first.utf8.count)
        try Data(text.utf8).write(to: file)

        let retained = try Diagnostics.read(file)
        XCTAssertTrue(retained.hasPrefix(first))
        XCTAssertFalse(retained.contains("previous entry"))
        XCTAssertEqual(retained.utf8.count, Diagnostics.limit)
    }

    func testFullDiagnosticsExportPreservesNewestGameFailureWithinBudget() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer {try? FileManager.default.removeItem(at: root)}
        let launcher = root.appendingPathComponent("launcher")
        let game = root.appendingPathComponent("game")
        try FileManager.default.createDirectory(at: launcher, withIntermediateDirectories: true)
        try FileManager.default.createDirectory(at: game.appendingPathComponent("logs"), withIntermediateDirectories: true)
        let marker = "[12:00:01] LATEST-GAME-FAILURE\n"
        let tail = Data(("\n" + marker).utf8)
        var full = Data(repeating: 65, count: Diagnostics.limit - tail.count)
        for index in stride(from: 79, to: full.count, by: 80) {full[index] = 10}
        full.append(tail)
        for url in [launcher.appendingPathComponent("launcher.log"), launcher.appendingPathComponent("launcher-previous.log"), game.appendingPathComponent("logs/latest.log"), game.appendingPathComponent("logs/previous.log")] {
            try full.write(to: url)
        }
        let export = try Diagnostics.export(folder: launcher, gameFolder: game)
        let data = try withExtendedLifetime(export) { try Data(contentsOf: export.url) }
        XCTAssertLessThanOrEqual(data.count, 4 * Diagnostics.limit)
        let text = String(decoding: data, as: UTF8.self)
        let current = try XCTUnwrap(text.components(separatedBy: "--- Game current ---\n").last)
        XCTAssertTrue(current.contains(marker), "Export must retain the newest error, not truncate it to fit headers")
    }

    func testShortLogsLeaveRoomForExpandedGameDiagnostics() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let launcher = root.appendingPathComponent("launcher")
        let game = root.appendingPathComponent("game")
        let logs = game.appendingPathComponent("logs")
        try FileManager.default.createDirectory(at: logs, withIntermediateDirectories: true)

        let first = "[12:00:00] [Render thread/INFO]: earliest useful line\n"
        let line = "[12:00:01] [Render thread/INFO]: " + String(repeating: "0.0.0.0 ", count: 64) + "\n"
        let input = first + String(repeating: line, count: (Diagnostics.limit - first.utf8.count) / line.utf8.count)
        try Data(input.utf8).write(to: logs.appendingPathComponent("latest.log"))

        let export = try Diagnostics.export(folder: launcher, gameFolder: game)
        let data = try withExtendedLifetime(export) { try Data(contentsOf: export.url) }
        let report = String(decoding: data, as: UTF8.self)
        XCTAssertGreaterThan(data.count, Diagnostics.limit)
        XCTAssertLessThanOrEqual(data.count, 4 * Diagnostics.limit)
        XCTAssertTrue(report.contains(first))
        XCTAssertFalse(report.contains("0.0.0.0"))
    }

    func testExportTailKeepsALineBeginningAtTheBudgetBoundary() {
        let marker = "[older lines omitted]\n"
        let recent = "Shader ready 🧪\n"
        let text = String(repeating: "old log line\n", count: 10) + recent
        XCTAssertEqual(
            Diagnostics.exportTail(text, maximum: marker.utf8.count + recent.utf8.count),
            marker + recent
        )
    }

    func testDiagnosticsCompressesSensitiveContinuationsWithoutLosingRecentEntries() {
        let original = "[12:00:00] [Render thread/INFO]: access_token:\n"
            + String(repeating: "\n", count: 80_000)
            + "opaque-fixture\n[12:00:01] [Render thread/INFO]: latest message\n"
        XCTAssertEqual(
            Diagnostics.redactExport(original),
            "[12:00:00] [Diagnostics/INFO]: [redacted sensitive log message]\n[12:00:01] [Render thread/INFO]: latest message\n"
        )
    }

    func testDiagnosticsExportRedactsFoldedFieldsAndPreRedactedContinuations() {
        let original = """
        2026-11-18T12:00:00Z Launcher ready
        2026-11-18T12:00:01Z token
          = folded-fixture-value
        2026-11-18T12:00:02Z [redacted sensitive log message]
          already-redacted-fixture-continuation
        2026-11-18T12:00:03Z Shader ready
        """ + "\n"
        let safe = Diagnostics.redactExport(original)
        XCTAssertFalse(safe.contains("fixture"))
        XCTAssertTrue(safe.contains("Z Launcher ready"))
        XCTAssertEqual(Diagnostics.redactExport(safe), safe, safe)
    }

    func testLauncherLogIndentsContinuationLinesThatLookLikeEntries() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        Diagnostics.record("Quoted output\n2026-11-18T12:00:00Z not a new entry", folder: root)
        let text = try Diagnostics.read(root.appendingPathComponent("launcher.log"))
        XCTAssertTrue(text.contains("\n  2026-11-18T12:00:00Z not a new entry\n"))
    }

    func testLauncherRotationWithNoRetainedEntriesKeepsThePreviousLog() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        let current = root.appendingPathComponent("launcher.log")
        let previous = root.appendingPathComponent("launcher-previous.log")
        let archived = Data("2026-11-18T05:00:00Z earlier diagnostics\n".utf8)
        try archived.write(to: previous)
        try Data(repeating: 120, count: Diagnostics.limit + 1).write(to: current)

        Diagnostics.record("New launch", folder: root)
        XCTAssertEqual(try Data(contentsOf: previous), archived)
        XCTAssertTrue(try String(contentsOf: current, encoding: .utf8).contains("New launch"))
    }

    func testLauncherRotationKeepsTheTailWhenRedactionExpandsIt() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        let manager = FileManager.default
        try manager.createDirectory(at: root, withIntermediateDirectories: true)
        defer { try? manager.removeItem(at: root) }
        let current = root.appendingPathComponent("launcher.log")
        let previous = root.appendingPathComponent("launcher-previous.log")
        let marker = "2026-11-18T05:00:00Z latest-before-rotation\n"
        let line = "2026-11-18T04:59:00Z " + String(repeating: "0.0.0.0 ", count: 64) + "\n"
        let repeats = (Diagnostics.limit - marker.utf8.count) / line.utf8.count
        let padding = String(repeating: "x", count: Diagnostics.limit - repeats * line.utf8.count - marker.utf8.count)
        let full = String(repeating: line, count: repeats) + padding + marker
        try Data(full.utf8).write(to: current)
        Diagnostics.record("current message", folder: root)

        let rotated = try Data(contentsOf: previous)
        XCTAssertLessThanOrEqual(rotated.count, Diagnostics.limit)
        XCTAssertTrue(String(decoding: rotated, as: UTF8.self).contains("latest-before-rotation"))
    }

    func testLauncherRotationKeepsUsefulEntriesAroundAnOldSensitiveMessage() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        let current = root.appendingPathComponent("launcher.log")
        let previous = root.appendingPathComponent("launcher-previous.log")
        let old = """
        2026-11-17T12:00:00Z First shader failure
        2026-11-17T12:00:01Z access_token:
        short-fixture-value
        2026-11-17T12:00:02Z Last shader failure
        """ + "\n"
        let filler = String(repeating: "x", count: Diagnostics.limit - old.utf8.count - 1)
        try Data((filler + "\n" + old).utf8).write(to: current)

        Diagnostics.record("New launch", folder: root)
        let rotated = try String(contentsOf: previous, encoding: .utf8)
        let next = try String(contentsOf: current, encoding: .utf8)
        XCTAssertTrue(rotated.contains("[oversized log line omitted]"))
        XCTAssertTrue(rotated.contains("First shader failure"))
        XCTAssertTrue(rotated.contains("Last shader failure"))
        XCTAssertFalse(rotated.contains("short-fixture-value"))
        XCTAssertTrue(next.contains("New launch"))
        XCTAssertFalse(next.contains("Last shader failure"))
        XCTAssertLessThanOrEqual(rotated.utf8.count, Diagnostics.limit)
    }

    func testLauncherKeepsRecordingWhenLogArchiveCannotBeWritten() throws {
        let folder = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: folder) }
        let previous = folder.appendingPathComponent("launcher-previous.log")
        try FileManager.default.createDirectory(at: previous, withIntermediateDirectories: true)
        let kept = previous.appendingPathComponent("keep")
        try Data("older data".utf8).write(to: kept)
        let latest = folder.appendingPathComponent("launcher.log")
        let oldest = "2026-11-18T12:00:00Z oldest entry\n"
        let recent = "2026-11-18T12:00:02Z keep this recent entry\n"
        let line = "2026-11-18T12:00:01Z " + String(repeating: "fixture ", count: 45) + "\n"
        let count = (Diagnostics.limit - oldest.utf8.count - recent.utf8.count) / line.utf8.count
        try Data((oldest + String(repeating: line, count: count) + recent).utf8).write(to: latest)

        Diagnostics.record("newest failure " + String(repeating: "detail ", count: 100), folder: folder)
        Diagnostics.record("still recording", folder: folder)

        let current = try String(contentsOf: latest, encoding: .utf8)
        XCTAssertTrue(current.contains(recent))
        XCTAssertTrue(current.contains("newest failure"))
        XCTAssertTrue(current.contains("still recording"))
        XCTAssertFalse(current.contains(oldest))
        XCTAssertLessThanOrEqual(current.utf8.count, Diagnostics.limit)
        XCTAssertEqual(try String(contentsOf: kept, encoding: .utf8), "older data")
    }

    func testDiagnosticExportReadsTheEngineLogHome() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let game = root.appendingPathComponent("custom player")
        let logs = game.appendingPathComponent("logs")
        try FileManager.default.createDirectory(at: logs, withIntermediateDirectories: true)
        try Data("[10:00:00] [Render/INFO]: game log fixture\n".utf8)
            .write(to: logs.appendingPathComponent("latest.log"))

        let resolved = try XCTUnwrap(Diagnostics.gameLogHome(fromEngine: ["path": game.path]))
        XCTAssertEqual(resolved.path, game.path)
        XCTAssertNil(Diagnostics.gameLogHome(fromEngine: ["path": "relative"]))
        XCTAssertNil(Diagnostics.gameLogHome(fromEngine: [:]))
        let snapshot = try Diagnostics.export(folder: root.appendingPathComponent("launcher"), gameFolder: resolved)
        let text = try withExtendedLifetime(snapshot) { try String(contentsOf: snapshot.url, encoding: .utf8) }
        XCTAssertTrue(text.contains("game log fixture"))
        XCTAssertFalse(text.contains(game.path))
    }

    func testDiagnosticExportReportsAnUnconfirmedGameLogFlush() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let logs = root.appendingPathComponent("logs")
        try FileManager.default.createDirectory(at: logs, withIntermediateDirectories: true)
        try Data("persisted game message\n".utf8).write(to: logs.appendingPathComponent("latest.log"))

        let export = try Diagnostics.export(folder: root, gameFolder: root, gameLogMayBeIncomplete: true)
        let report = try withExtendedLifetime(export) { try String(contentsOf: export.url, encoding: .utf8) }
        XCTAssertTrue(report.contains("The game log writer could not confirm that recent messages were saved."))
        XCTAssertTrue(report.contains("persisted game message"))
    }

    func testDiagnosticLogHomeFallsBackForAnEmptyEnvironmentOverride() {
        let data = URL(fileURLWithPath: "/fixtures/app-support", isDirectory: true)
        let expected = data.appendingPathComponent("horizon", isDirectory: true)
        XCTAssertEqual(Diagnostics.gameLogHome(override: nil, dataDirectory: data), expected)
        XCTAssertEqual(Diagnostics.gameLogHome(override: "", dataDirectory: data), expected)
        XCTAssertEqual(
            Diagnostics.gameLogHome(override: "/fixtures/game", dataDirectory: data),
            URL(fileURLWithPath: "/fixtures/game", isDirectory: true)
        )
    }

    func testDiagnosticsExportKeepsReadableLogsWhenAnotherLogCannotBeOpened() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let launcher = root.appendingPathComponent("launcher")
        let game = root.appendingPathComponent("game")
        let logs = game.appendingPathComponent("logs")
        try FileManager.default.createDirectory(at: launcher.appendingPathComponent("launcher-previous.log"),
                                                withIntermediateDirectories: true)
        try FileManager.default.createDirectory(at: logs, withIntermediateDirectories: true)
        Diagnostics.record("launcher ready", folder: launcher)
        try Data("[12:00:01] shader failed\naccess_token=private-fixture\n".utf8)
            .write(to: logs.appendingPathComponent("latest.log"))

        let export = try Diagnostics.export(folder: launcher, gameFolder: game)
        let report = try withExtendedLifetime(export) { try String(contentsOf: export.url, encoding: .utf8) }
        XCTAssertTrue(report.contains("--- Launcher previous ---\nLog unavailable during export.\n"))
        XCTAssertTrue(report.contains("--- Game previous ---\nNo log recorded yet.\n"))
        XCTAssertTrue(report.contains("launcher ready"))
        XCTAssertTrue(report.contains("shader failed"))
        XCTAssertFalse(report.contains("private-fixture"))
    }

    func testDiagnosticsAreBoundedRedactedAndExportOnlyLogs() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer {try? FileManager.default.removeItem(at:root)}
        let launcher = root.appendingPathComponent("launcher")
        let game = root.appendingPathComponent("game")
        try FileManager.default.createDirectory(at:game.appendingPathComponent("logs"),withIntermediateDirectories:true)
        try Data("private-account-fixture".utf8).write(to:game.appendingPathComponent("accounts.json"))
        for text in ["Authorization: Bearer confidential-value", "refresh_token:\nshort-value", "cookie=abc"] {
            XCTAssertTrue(Diagnostics.redact(text).contains("redacted"))
        }
        XCTAssertTrue(Diagnostics.redact("shader: unexpected token in vertex stage").contains("unexpected token"))
        for _ in 0..<90 {Diagnostics.record(String(repeating:"shader diagnostic ",count:1000),folder:launcher)}
        for name in ["launcher.log","launcher-previous.log"] {
            XCTAssertLessThanOrEqual(try Data(contentsOf:launcher.appendingPathComponent(name)).count,Diagnostics.limit)
        }
        try Data("shader Complementary: pipeline failed\naccess_token=confidential-value\n".utf8).write(to:game.appendingPathComponent("logs/latest.log"))
        let result = try Diagnostics.export(folder:launcher,gameFolder:game)
        let bytes = try withExtendedLifetime(result) { try Data(contentsOf: result.url) }
        XCTAssertLessThanOrEqual(bytes.count,4*Diagnostics.limit)
        let text = String(decoding:bytes,as:UTF8.self)
        XCTAssertTrue(text.contains("Complementary: pipeline failed"))
        XCTAssertFalse(text.contains("confidential-value"))
        XCTAssertFalse(text.contains("private-account-fixture"))
    }

    func testDiagnosticExportDoesNotReadGameLogsDuringRotation() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let game = root.appendingPathComponent("game")
        let logs = game.appendingPathComponent("logs")
        try FileManager.default.createDirectory(at: logs, withIntermediateDirectories: true)
        let latest = logs.appendingPathComponent("latest.log")
        try Data("unstable game log".utf8).write(to: latest)
        Diagnostics.record("launcher remains available", folder: root)
        let writer = try FileHandle(forUpdating: latest)
        defer { try? writer.close() }
        XCTAssertEqual(flock(writer.fileDescriptor, LOCK_EX | LOCK_NB), 0)

        let blocked = try Diagnostics.export(folder: root, gameFolder: game)
        try withExtendedLifetime(blocked) {
            let text = try String(contentsOf: blocked.url, encoding: .utf8)
            XCTAssertTrue(text.contains("Game logs are rotating"))
            XCTAssertTrue(text.contains("launcher remains available"))
            XCTAssertFalse(text.contains("unstable game log"))
        }
        XCTAssertEqual(flock(writer.fileDescriptor, LOCK_UN), 0)
        let stable = try Diagnostics.export(folder: root, gameFolder: game)
        try withExtendedLifetime(stable) {
            XCTAssertTrue(try String(contentsOf: stable.url, encoding: .utf8).contains("unstable game log"))
        }
    }

    func testOverlappingDiagnosticExportsKeepIndependentSnapshotsAndCleanUp() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let game = root.appendingPathComponent("game")
        let logs = game.appendingPathComponent("logs")
        try FileManager.default.createDirectory(at: logs, withIntermediateDirectories: true)
        let latest = logs.appendingPathComponent("latest.log")
        try Data("First snapshot\n".utf8).write(to: latest)
        var first: DiagnosticExport? = try Diagnostics.export(folder: root, gameFolder: game)
        let firstURL = try XCTUnwrap(first?.url)
        try Data("Second snapshot\n".utf8).write(to: latest)
        let second = try Diagnostics.export(folder: root, gameFolder: game)

        XCTAssertNotEqual(firstURL, second.url)
        XCTAssertEqual(firstURL.lastPathComponent, second.url.lastPathComponent)
        XCTAssertTrue(try String(contentsOf: firstURL, encoding: .utf8).contains("First snapshot"))
        first = nil
        XCTAssertFalse(FileManager.default.fileExists(atPath: firstURL.deletingLastPathComponent().path))
        try withExtendedLifetime(second) {
            XCTAssertTrue(try String(contentsOf: second.url, encoding: .utf8).contains("Second snapshot"))
        }
    }

    func testColorsDecorationsAndReset() {
        let r = MinecraftFormatting.runs("§aGreen §lBold§oItalic§nUnder§mStrike§rPlain")
        XCTAssertEqual(r[0].style.color, 0x55FF55)
        XCTAssertTrue(r[1].style.bold)
        XCTAssertTrue(r[2].style.italic)
        XCTAssertTrue(r[3].style.underline)
        XCTAssertTrue(r[4].style.strike)
        XCTAssertEqual(r.last?.style, MinecraftFormatting.Style())
        XCTAssertEqual(MinecraftFormatting.plain("§6§lHYPIXEL§r\n§7SkyBlock"), "HYPIXEL\nSkyBlock")
    }
    func testColorResetsDecorationAndRGBIsOneColor() {
        let r = MinecraftFormatting.runs("§lBold§cRed§x§1§2§3§4§5§6Hex")
        XCTAssertFalse(r[1].style.bold)
        XCTAssertEqual(r[1].style.color, 0xFF5555)
        XCTAssertEqual(r[2].style.color, 0x123456)
    }
    func testUnicodeUnknownSequencesAndTrailingSectionSignArePreserved() {
        XCTAssertEqual(MinecraftFormatting.plain("§b世界 🧱 §z literal §"), "世界 🧱 §z literal §")
        XCTAssertTrue(MinecraftFormatting.runs("§kSecret")[0].style.obfuscated)
        XCTAssertEqual(MinecraftFormatting.plain("§LUPPER"), "UPPER")
    }
    func testWorldCreationJSONCarriesAllFields() throws {
        var settings = WorldCreationSettings()
        settings.name = "A world with spaces"; settings.seed = "hello world"; settings.mode = "hardcore"
        settings.flat = true; settings.structures = false
        let data = try JSONEncoder().encode(settings)
        let json = try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any])
        XCTAssertEqual(json["name"] as? String, settings.name)
        XCTAssertEqual(json["seed"] as? String, "hello world")
        XCTAssertEqual(json["mode"] as? String, "hardcore")
        XCTAssertEqual(json["flat"] as? Bool, true)
        XCTAssertEqual(json["structures"] as? Bool, false)
        XCTAssertEqual(json["bonus_chest"] as? Bool, false)
    }
}

@MainActor
final class SettingsModelTests: XCTestCase {
    func testEmptyAndFailedResponsesAreVisibleFailures() async {
        let empty = GameOptions(request: { _ in ["ok": true] })
        await empty.load()
        XCTAssertNotNil(empty.issue)
        XCTAssertTrue(empty.options.isEmpty)
        XCTAssertFalse(empty.loading)
        let failed = GameOptions(request: { _ in ["error": "Disconnected"] })
        await failed.load()
        XCTAssertEqual(failed.issue, "Disconnected")
    }
    func testPartialOptionsRemainUsableAndIdentifyMissingControls() async {
        let model = GameOptions(request: { _ in ["options": ["autoJump": ["type": "bool", "value": false]]] })
        await model.load()
        XCTAssertNil(model.issue)
        XCTAssertNotNil(model.options["autoJump"])
        XCTAssertTrue(model.unavailable.contains("Graphics"))
    }
    func testToggleUpdatesOnlyAfterAcknowledgement() async {
        var value = false
        let model = GameOptions(request: { line in
            if line.hasPrefix("options-set autoJump ") {
                value = line.hasSuffix("true")
                return ["option": ["type": "bool", "value": value]]
            }
            return ["options": ["autoJump": ["type": "bool", "value": value]]]
        })
        await model.load()
        let issue = await model.set("autoJump", "true")
        XCTAssertNil(issue)
        XCTAssertTrue(model.options["autoJump"]?.flag == true)
        await model.load()
        XCTAssertTrue(model.options["autoJump"]?.flag == true)
        let noAck = GameOptions(request: { _ in ["ok": true] })
        let noAckIssue = await noAck.set("autoJump", "true")
        XCTAssertNotNil(noAckIssue)
    }
    func testPreviewFixtureCoversEveryLauncherSetting() throws {
        let url = try XCTUnwrap(Bundle.main.url(forResource: "EngineOptions", withExtension: "json"))
        let object = try XCTUnwrap(JSONSerialization.jsonObject(with: Data(contentsOf: url)) as? [String: Any])
        let options = try XCTUnwrap(object["options"] as? [String: Any])
        XCTAssertEqual(Set(options.keys), Set(GameOptions.groups.flatMap { $0.rows.map(\.key) }))
    }
}

private func temporarilyRemoveResourceMarker() -> () -> Void {
    let marker = GameFolder.resources.appendingPathComponent("horizon-resources.json")
    let previous = try? Data(contentsOf: marker)
    try? FileManager.default.removeItem(at: marker)
    return {
        if let previous { try? previous.write(to: marker) }
        else { try? FileManager.default.removeItem(at: marker) }
    }
}

private func installFixtureResourceMarker() throws {
    try FileManager.default.createDirectory(at: GameFolder.resources, withIntermediateDirectories: true)
    try Data("{}".utf8).write(to: GameFolder.resources.appendingPathComponent("horizon-resources.json"))
}

@MainActor
final class LauncherAsyncTests: XCTestCase {
    func testLaunchCleanupRemovesOnlyAbandonedPrivateStagingArchives() throws {
        let files = FileManager.default
        let directory = files.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try files.createDirectory(at: directory, withIntermediateDirectories: false)
        defer { try? files.removeItem(at: directory) }

        let abandoned = directory.appendingPathComponent("horizon-download-\(UUID().uuidString).zip")
        let packArchive = directory.appendingPathComponent("horizon-pack-download-\(UUID().uuidString).zip")
        let otherZip = directory.appendingPathComponent("horizon-download-my-world.zip")
        let folder = directory.appendingPathComponent("horizon-download-\(UUID().uuidString).zip")
        let symlink = directory.appendingPathComponent("horizon-download-\(UUID().uuidString).zip")
        let bytes = Data("archive fixture".utf8)
        try bytes.write(to: abandoned)
        try bytes.write(to: packArchive)
        try bytes.write(to: otherZip)
        try files.createDirectory(at: folder, withIntermediateDirectories: false)
        try files.createSymbolicLink(at: symlink, withDestinationURL: otherZip)

        ArchiveStaging.removeAbandoned(in: directory, prefix: "horizon-download-")
        XCTAssertTrue(files.fileExists(atPath: packArchive.path))
        ArchiveStaging.removeAbandoned(in: directory, prefix: "horizon-pack-download-")
        XCTAssertFalse(files.fileExists(atPath: packArchive.path))

        XCTAssertFalse(files.fileExists(atPath: abandoned.path))
        XCTAssertEqual(try Data(contentsOf: otherZip), bytes)
        XCTAssertEqual(try folder.resourceValues(forKeys: [.isDirectoryKey]).isDirectory, true)
        XCTAssertEqual(try symlink.resourceValues(forKeys: [.isSymbolicLinkKey]).isSymbolicLink, true)
    }

    func testStagingLimitsActualFileBytesWithoutChangingTheOriginal() throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: false)
        defer { try? FileManager.default.removeItem(at: directory) }
        let source = directory.appendingPathComponent("provider.zip")
        let staged = directory.appendingPathComponent("staged.zip")
        let bytes = Data(repeating: 0x2a, count: (1 << 20) + 32)
        try bytes.write(to: source)
        XCTAssertThrowsError(try GameFilesDownload.stageArchive(
            from: source, to: staged, maximumBytes: Int64(bytes.count - 1)
        )) { error in
            XCTAssertEqual((error as? CocoaError)?.code, .fileReadTooLarge)
            XCTAssertEqual(GameFilesDownload.fileImportMessage(error), "The archive exceeds the 2 GiB game-file limit.")
            XCTAssertTrue(GameFilesDownload.errorFacts(error).contains("kind=archive_too_large"))
        }
        XCTAssertEqual(try staged.resourceValues(forKeys: [.fileSizeKey]).fileSize, 1 << 20)
        XCTAssertEqual(try Data(contentsOf: source), bytes)
        try FileManager.default.removeItem(at: staged)
        try GameFilesDownload.stageArchive(from: source, to: staged, maximumBytes: Int64(bytes.count))
        XCTAssertEqual(try Data(contentsOf: staged), bytes)
    }

    func testStagingKeepsTheFilesystemErrorWhenTheTemporaryDirectoryIsMissing() throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: false)
        defer { try? FileManager.default.removeItem(at: directory) }
        let source = directory.appendingPathComponent("provider.zip")
        let staged = directory.appendingPathComponent("missing/staged.zip")
        try Data("provider file".utf8).write(to: source)

        XCTAssertThrowsError(try GameFilesDownload.stageArchive(from: source, to: staged, maximumBytes: 64)) {
            XCTAssertEqual(($0 as? CocoaError)?.code, .fileNoSuchFile)
        }
    }

    func testPreparedFileStagesOutsideManualScanAndCleansOnlyItsCopy() async throws {
        let restore = temporarilyRemoveResourceMarker()
        defer { restore() }
        let source = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        let bytes = Data("private import staging fixture".utf8)
        try bytes.write(to: source)
        defer { try? FileManager.default.removeItem(at: source) }
        var staged: URL?
        let model = GameFilesDownload(importArchive: { line in
            XCTAssertTrue(line.hasPrefix("import-game-files "))
            let file = URL(fileURLWithPath: String(line.dropFirst("import-game-files ".count)))
            staged = file
            XCTAssertNotEqual(file.deletingLastPathComponent().resolvingSymlinksInPath(),
                              GameFolder.documents.resolvingSymlinksInPath())
            XCTAssertEqual(try? Data(contentsOf: file), bytes)
            do { try installFixtureResourceMarker() }
            catch {
                XCTFail("Could not install fixture marker: \(error)")
                return ["error": "Fixture marker was not installed"]
            }
            return ["ok": true, "kind": "resources"]
        })
        await model.importPreparedFile(source)
        XCTAssertNotNil(staged)
        if let staged { XCTAssertFalse(FileManager.default.fileExists(atPath: staged.path)) }
        XCTAssertEqual(try Data(contentsOf: source), bytes)
        if case .done = model.state {} else { XCTFail("Import did not finish: \(model.state)") }
    }

    private func hits(_ id: String) -> [String: Any] { ["hits": [["project_id": id, "title": id]]] }

    func testCancelledPackSearchLeavesCurrentResultsInPlace() async {
        var requests = 0
        let store = PackStore(fetch: { _ in
            requests += 1
            return self.hits("current")
        })
        await store.search(.resource, query: "current")
        let cancelled = Task { await store.search(.shader, query: "cancelled") }
        cancelled.cancel()
        await cancelled.value
        XCTAssertEqual(requests, 1)
        XCTAssertEqual(store.results.map(\.id), ["current"])
        XCTAssertFalse(store.searching)
    }

    func testCancellingAnInFlightPackSearchPreservesThePreviousResults() async {
        let started = expectation(description: "Second search has started")
        var pending: CheckedContinuation<Any?, Never>?
        let store = PackStore(fetch: { url in
            let query = URLComponents(url: url, resolvingAgainstBaseURL: false)?
                .queryItems?.first(where: { $0.name == "query" })?.value
            if query == "current" { return self.hits("current") }
            return await withCheckedContinuation {
                pending = $0
                started.fulfill()
            }
        })
        await store.search(.resource, query: "current")
        let search = Task { await store.search(.resource, query: "second") }
        await fulfillment(of: [started], timeout: 3)
        search.cancel()
        pending?.resume(returning: hits("late"))
        await search.value
        XCTAssertFalse(store.searching)
        XCTAssertEqual(store.results.map(\.id), ["current"])
    }

    func testCancellingPackInstallWhileAwaitingVersionsDoesNotStartTransfer() async {
        let requested = expectation(description: "pack version lookup started")
        var reply: CheckedContinuation<Any?, Never>?
        var transfers = 0
        let store = PackStore(fetch: { _ in
            await withCheckedContinuation {
                reply = $0
                requested.fulfill()
            }
        }, transfer: { _, _ in
            transfers += 1
            throw URLError(.cannotConnectToHost)
        })
        let project = ModrinthProject(id: "fixture", kind: .shader, title: "Fixture", summary: "",
                                      author: "", downloads: 0, icon: nil, gameVersions: [])
        let task = Task { await store.install(project, version: "26.3") }
        await fulfillment(of: [requested], timeout: 2)
        task.cancel()
        reply?.resume(returning: [["game_versions": ["26.3"], "files": [
            ["filename": "shader.zip", "url": "https://example.invalid/shader.zip",
             "size": 1, "hashes": ["sha512": String(repeating: "f", count: 128)]]
        ]]])
        switch await task.value {
        case .success: XCTFail("Cancelled install succeeded")
        case let .failure(error): XCTAssertEqual(error.message, "The pack download was cancelled.")
        }
        XCTAssertEqual(transfers, 0)
        XCTAssertNil(store.downloading[project.id])
    }

    func testFailedVersionLookupIsDistinguishedFromAPackWithoutFiles() async {
        let project = ModrinthProject(id: "fixture", kind: .resource, title: "Fixture", summary: "",
                                      author: "", downloads: 0, icon: nil, gameVersions: [])
        let responses: [(Any?, String)] = [
            (nil, "Could not read the pack's versions from Modrinth. Try again."),
            ([], "Fixture has no file to download."),
        ]
        for (response, message) in responses {
            let store = PackStore(fetch: { _ in response })
            switch await store.install(project, version: "26.3") {
            case .success: XCTFail("Install required a valid file")
            case let .failure(error): XCTAssertEqual(error.message, message)
            }
            XCTAssertNil(store.downloading[project.id])
        }
    }

    func testOldPackSearchCannotOverwriteNewResults() async {
        var pending: [String: CheckedContinuation<Any?, Never>] = [:]
        let store = PackStore(fetch: { url in
            let query = URLComponents(url: url, resolvingAgainstBaseURL: false)?.queryItems?.first { $0.name == "query" }?.value ?? ""
            return await withCheckedContinuation { pending[query] = $0 }
        })
        let old = Task { await store.search(.resource, query: "old") }
        while pending["old"] == nil { await Task.yield() }
        let new = Task { await store.search(.shader, query: "new") }
        while pending["new"] == nil { await Task.yield() }
        pending.removeValue(forKey: "new")?.resume(returning: hits("new"))
        await new.value
        pending.removeValue(forKey: "old")?.resume(returning: hits("old"))
        await old.value
        XCTAssertEqual(store.results.map(\.id), ["new"])
        XCTAssertFalse(store.searching)
        XCTAssertNil(store.failed)
    }

    func testOldPackFailureCannotEndNewSearchOrSetError() async {
        var pending: [String: CheckedContinuation<Any?, Never>] = [:]
        let store = PackStore(fetch: { url in
            let query = URLComponents(url: url, resolvingAgainstBaseURL: false)?.queryItems?.first { $0.name == "query" }?.value ?? ""
            return await withCheckedContinuation { pending[query] = $0 }
        })
        let old = Task { await store.search(.resource, query: "old") }
        while pending["old"] == nil { await Task.yield() }
        let new = Task { await store.search(.shader, query: "new") }
        while pending["new"] == nil { await Task.yield() }
        pending.removeValue(forKey: "old")?.resume(returning: nil)
        await old.value
        XCTAssertTrue(store.searching)
        XCTAssertNil(store.failed)
        pending.removeValue(forKey: "new")?.resume(returning: hits("new"))
        await new.value
        XCTAssertEqual(store.results.map(\.id), ["new"])
    }

    func testDuplicatePackInstallDoesNotStartAnotherRequest() async {
        var resume: CheckedContinuation<Any?, Never>?
        let store = PackStore(fetch: { _ in await withCheckedContinuation { resume = $0 } })
        let project = ModrinthProject(id: "test", kind: .resource, title: "Test", summary: "", author: "", downloads: 0, icon: nil, gameVersions: [])
        let first = Task { await store.install(project, version: "26.3") }
        while resume == nil { await Task.yield() }
        if case .success = await store.install(project, version: "26.3") { XCTFail("Duplicate install succeeded") }
        XCTAssertNotNil(store.downloading[project.id])
        resume?.resume(returning: [])
        _ = await first.value
        XCTAssertNil(store.downloading[project.id])
    }

    func testConcurrentPackStoresReuseTheFirstVerifiedCopy() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }
        let original = root.appendingPathComponent("shared.zip")
        let bytes = Data(repeating: 1, count: 2 << 20)
        let different = Data(repeating: 2, count: bytes.count)
        try different.write(to: original)
        let hash = SHA512.hash(data: bytes).map { String(format: "%02x", $0) }.joined()
        let url = try XCTUnwrap(URL(string: "https://example.invalid/shared.zip"))
        let response = try XCTUnwrap(HTTPURLResponse(url: url, statusCode: 200, httpVersion: nil, headerFields: nil))
        let versions: [[String: Any]] = [[
            "game_versions": ["26.3"], "files": [[
                "filename": "shared.zip", "url": url.absoluteString, "primary": true,
                "size": bytes.count, "hashes": ["sha512": hash],
            ]],
        ]]
        let transferring = expectation(description: "both stores have a download in flight")
        transferring.expectedFulfillmentCount = 2
        var pending: [CheckedContinuation<Void, Never>] = []
        var released = false
        let transfer: PackStore.Transfer = { _, _ in
            let file = root.appendingPathComponent(UUID().uuidString + ".tmp")
            try bytes.write(to: file)
            if !released {
                await withCheckedContinuation {
                    pending.append($0)
                    transferring.fulfill()
                }
            }
            return (file, response)
        }
        let project = ModrinthProject(id: "pack", kind: .resource, title: "Pack", summary: "",
                                      author: "", downloads: 0, icon: nil, gameVersions: [])
        let firstStore = PackStore(fetch: { _ in versions }, transfer: transfer, folder: { _ in root })
        let secondStore = PackStore(fetch: { _ in versions }, transfer: transfer, folder: { _ in root })
        let first = Task { await firstStore.install(project, version: "26.3") }
        let second = Task { await secondStore.install(project, version: "26.3") }
        await fulfillment(of: [transferring], timeout: 5)
        released = true
        pending.forEach { $0.resume() }
        let results = try await [first.value.get(), second.value.get()]
        XCTAssertEqual(results.map(\.file), ["shared (2).zip", "shared (2).zip"])
        XCTAssertEqual(results.filter(\.reused).count, 1)
        XCTAssertEqual(try Data(contentsOf: original), different)
        XCTAssertEqual(try Set(FileManager.default.contentsOfDirectory(atPath: root.path)), ["shared.zip", "shared (2).zip"])
    }

    func testPackInstallPrefersAUsableCompatibleZipAndSkipsAttachments() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        let bytes = Data("pack fixture".utf8)
        let hash = SHA512.hash(data: bytes).map { String(format: "%02x", $0) }.joined()
        func file(_ name: String, primary: Bool = false, type: String? = nil,
                  checksum: String? = nil) -> [String: Any] {
            var metadata: [String: Any] = [
                "filename": name, "url": "https://example.invalid/\(name)", "primary": primary,
                "size": bytes.count, "hashes": ["sha512": checksum ?? hash],
            ]
            metadata["file_type"] = type
            return metadata
        }
        let versions: [[String: Any]] = [
            ["game_versions": ["26.4"], "files": [file("other.zip", primary: true)]],
            ["game_versions": ["26.3"], "files": [file("unverified.zip", primary: true, checksum: "")]],
            ["game_versions": ["26.3"], "files": [
                file("README.txt", primary: true),
                file("companion.zip", type: "required-resource-pack"),
                file("pack.zip"),
            ]],
        ]
        let project = ModrinthProject(id: "fixture", kind: .shader, title: "Fixture", summary: "",
                                      author: "", downloads: 0, icon: nil, gameVersions: [])
        var requested: [String] = []
        let store = PackStore(fetch: { _ in versions }, transfer: { request, expectedSize in
            XCTAssertEqual(expectedSize, Int64(bytes.count))
            let url = try XCTUnwrap(request.url)
            requested.append(url.lastPathComponent)
            let temporary = root.appendingPathComponent(UUID().uuidString)
            try bytes.write(to: temporary)
            return (temporary, try XCTUnwrap(HTTPURLResponse(url: url, statusCode: 200,
                                                            httpVersion: nil, headerFields: nil)))
        }, folder: { root.appendingPathComponent($0.rawValue) })
        let compatible = try await store.install(project, version: "26.3").get()
        XCTAssertEqual(compatible.file, "pack.zip")
        XCTAssertNil(compatible.note)

        let fallback = try await store.install(project, version: "26.2").get()
        XCTAssertEqual(fallback.file, "other.zip")
        XCTAssertTrue(fallback.note?.contains("26.4") == true)
        XCTAssertEqual(requested, ["pack.zip", "other.zip"])
    }

    func testCancellingAfterTransferLeavesNoInstalledPack() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        let manager = FileManager.default
        try manager.createDirectory(at: root, withIntermediateDirectories: true)
        defer { try? manager.removeItem(at: root) }
        let download = root.appendingPathComponent("verified-download")
        let packs = root.appendingPathComponent("packs")
        let bytes = Data("verified pack fixture".utf8)
        let hash = SHA512.hash(data: bytes).map { String(format: "%02x", $0) }.joined()
        let url = try XCTUnwrap(URL(string: "https://example.invalid/pack.zip"))
        let response = try XCTUnwrap(HTTPURLResponse(url: url, statusCode: 200, httpVersion: nil, headerFields: nil))
        let staged = expectation(description: "transfer has staged the pack")
        var finish: CheckedContinuation<Void, Never>?
        let store = PackStore(fetch: { _ in
            [["game_versions": ["26.3"], "files": [
                ["filename": "pack.zip", "url": url.absoluteString, "size": bytes.count, "hashes": ["sha512": hash]]
            ]]]
        }, transfer: { _, _ in
            try bytes.write(to: download)
            await withCheckedContinuation {
                finish = $0
                staged.fulfill()
            }
            return (download, response)
        }, folder: { _ in packs })
        let project = ModrinthProject(id: "fixture", kind: .resource, title: "Fixture", summary: "",
                                      author: "", downloads: 0, icon: nil, gameVersions: [])
        let install = Task { await store.install(project, version: "26.3") }
        await fulfillment(of: [staged], timeout: 2)
        install.cancel()
        finish?.resume()
        switch await install.value {
        case .success: XCTFail("Cancelled install published its archive")
        case let .failure(error): XCTAssertEqual(error.message, "The pack download was cancelled.")
        }
        XCTAssertFalse(manager.fileExists(atPath: download.path))
        XCTAssertFalse(manager.fileExists(atPath: packs.path))
        XCTAssertNil(store.downloading[project.id])
    }

    func testPackPublicationSkipsAnOccupiedLinkAndKeepsOtherPacks() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        let manager = FileManager.default
        try manager.createDirectory(at: root, withIntermediateDirectories: true)
        defer { try? manager.removeItem(at: root) }
        let preferred = root.appendingPathComponent("shared.zip")
        let first = root.appendingPathComponent("shared (2).zip")
        let verified = root.appendingPathComponent("download")
        let missing = root.appendingPathComponent("missing.zip")
        try manager.createSymbolicLink(at: preferred, withDestinationURL: missing)
        try Data("first pack".utf8).write(to: first)
        try Data("verified pack".utf8).write(to: verified)

        let published = try PackStore.publishArchive(verified, preferred: preferred)
        XCTAssertEqual(published.lastPathComponent, "shared (3).zip")
        XCTAssertEqual(try manager.destinationOfSymbolicLink(atPath: preferred.path), missing.path)
        XCTAssertEqual(try Data(contentsOf: first), Data("first pack".utf8))
        XCTAssertEqual(try Data(contentsOf: published), Data("verified pack".utf8))
        XCTAssertFalse(manager.fileExists(atPath: verified.path))
    }

    func testLongPackFilenameCanBeRenamedAndReusedAfterACollision() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }
        let name = String(repeating: "a", count: 280) + ".zip"
        let original = root.appendingPathComponent(String(repeating: "a", count: 251) + ".zip")
        let existing = Data("original archive".utf8)
        try existing.write(to: original)
        let temporary = root.appendingPathComponent("download")
        let bytes = Data("new archive".utf8)
        let hash = SHA512.hash(data: bytes).map { String(format: "%02x", $0) }.joined()
        let url = try XCTUnwrap(URL(string: "https://example.invalid/pack.zip"))
        let response = try XCTUnwrap(HTTPURLResponse(url: url, statusCode: 200, httpVersion: nil, headerFields: nil))
        var transfers = 0
        let store = PackStore(fetch: { _ in
            [["game_versions": ["26.3"], "files": [
                ["filename": name, "url": url.absoluteString, "size": bytes.count, "hashes": ["sha512": hash]]
            ]]]
        }, transfer: { _, _ in
            transfers += 1
            try bytes.write(to: temporary)
            return (temporary, response)
        }, folder: { _ in root })
        let project = ModrinthProject(id: "fixture", kind: .shader, title: "Fixture", summary: "",
                                      author: "", downloads: 0, icon: nil, gameVersions: [])

        let installed = try await store.install(project, version: "26.3").get()
        XCTAssertEqual(installed.file, String(repeating: "a", count: 247) + " (2).zip")
        XCTAssertEqual(try Data(contentsOf: original), existing)
        XCTAssertEqual(try Data(contentsOf: root.appendingPathComponent(installed.file)), bytes)
        let reused = try await store.install(project, version: "26.3").get()
        XCTAssertTrue(reused.reused)
        XCTAssertEqual(reused.file, installed.file)
        XCTAssertEqual(transfers, 1)
    }

    func testOversizedShaderIsRejectedBeforeTransfer() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        let store = PackStore(fetch: { _ in
            [["game_versions": ["26.3"], "files": [[
                "filename": "large.zip", "url": "https://example.invalid/large.zip",
                "size": (512 << 20) + 1, "hashes": ["sha512": String(repeating: "a", count: 128)],
            ]]]]
        }, transfer: { _, _ in
            XCTFail("An oversized shader must be rejected before transfer")
            throw PackError("Unexpected download")
        }, folder: { _ in root })
        let project = ModrinthProject(id: "large", kind: .shader, title: "Large", summary: "",
                                      author: "", downloads: 0, icon: nil, gameVersions: [])
        switch await store.install(project, version: "26.3") {
        case .success: XCTFail("The engine cannot read this shader archive")
        case let .failure(error): XCTAssertTrue(error.message.contains("512 MiB"))
        }
        XCTAssertFalse(FileManager.default.fileExists(atPath: root.path))
    }

    func testPackDownloadKeepsItsSearchKindAndDoesNotReplaceAnExistingFile() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let shaders = root.appendingPathComponent("shader")
        let resources = root.appendingPathComponent("resource")
        try FileManager.default.createDirectory(at: shaders, withIntermediateDirectories: true)
        try FileManager.default.createDirectory(at: resources, withIntermediateDirectories: true)
        let existingShader = shaders.appendingPathComponent("shared.zip")
        let existingResource = resources.appendingPathComponent("shared.zip")
        try Data("existing shader".utf8).write(to: existingShader)
        try Data("existing resource".utf8).write(to: existingResource)
        let temporary = root.appendingPathComponent("download")
        let downloaded = Data("new shader fixture".utf8)
        let sha512 = SHA512.hash(data: downloaded).map { String(format: "%02X", $0) }.joined()
        var hashes = ["sha512": sha512]
        var transfers = 0
        try downloaded.write(to: temporary)
        let store = PackStore(fetch: { url in
            if url.path.hasSuffix("/search") {
                return ["hits": [["project_id": "fixture", "title": "Fixture"]]]
            }
            return [["game_versions": ["26.3"], "files": [
                ["filename": "shared.zip", "url": "https://example.invalid/shared.zip", "primary": true,
                 "size": downloaded.count, "hashes": hashes],
            ]]]
        }, transfer: { request, expectedSize in
            XCTAssertEqual(expectedSize, Int64(downloaded.count))
            transfers += 1
            let url = try XCTUnwrap(request.url)
            return (temporary, try XCTUnwrap(HTTPURLResponse(url: url, statusCode: 200, httpVersion: nil, headerFields: nil)))
        }, folder: { root.appendingPathComponent($0.rawValue) })

        await store.search(.shader, query: "")
        let shader = try XCTUnwrap(store.results.first)
        await store.search(.resource, query: "")
        XCTAssertEqual(store.results.first?.kind, .resource)
        let first = await store.install(shader, version: "26.3")
        if case let .failure(error) = first { XCTFail(error.message) }
        let installed = try first.get()
        XCTAssertEqual(installed.file, "shared (2).zip")
        let installedURL = shaders.appendingPathComponent(installed.file)
        XCTAssertEqual(try Data(contentsOf: installedURL), downloaded)
        XCTAssertFalse(installed.reused)
        XCTAssertEqual(try Data(contentsOf: existingShader), Data("existing shader".utf8))
        XCTAssertEqual(try Data(contentsOf: existingResource), Data("existing resource".utf8))
        XCTAssertFalse(FileManager.default.fileExists(atPath: temporary.path))

        var corrupted = downloaded
        corrupted[0] ^= 1
        // A same-size local file with different bytes must still go through
        // the download and its independent verification.
        try corrupted.write(to: installedURL)
        for invalid in [corrupted, Data(downloaded.dropLast())] {
            try invalid.write(to: temporary)
            switch await store.install(shader, version: "26.3") {
            case .success: XCTFail("An unverified pack was installed")
            case let .failure(error): XCTAssertTrue(error.message.contains("checksum or size"))
            }
            XCTAssertFalse(FileManager.default.fileExists(atPath: temporary.path))
            XCTAssertEqual(try Set(FileManager.default.contentsOfDirectory(atPath: shaders.path)),
                           ["shared.zip", "shared (2).zip"])
        }
        hashes = ["sha1": Insecure.SHA1.hash(data: downloaded).map { String(format: "%02x", $0) }.joined()]
        // The first installed copy may have a numbered name even after the
        // conflict that originally required the suffix has been removed.
        try FileManager.default.removeItem(at: existingShader)
        try downloaded.write(to: installedURL)
        XCTAssertEqual(transfers, 3)
        let legacy = try await store.install(shader, version: "26.3").get()
        XCTAssertEqual(legacy.file, "shared (2).zip")
        XCTAssertTrue(legacy.reused)
        XCTAssertEqual(transfers, 3)
        XCTAssertEqual(try Data(contentsOf: shaders.appendingPathComponent(legacy.file)), downloaded)
        XCTAssertEqual(try FileManager.default.contentsOfDirectory(atPath: shaders.path), ["shared (2).zip"])
        XCTAssertFalse(FileManager.default.fileExists(atPath: temporary.path))
    }

    func testStaleGameFileCompletionDoesNotImportOrReplaceSharedArchive() async throws {
        let shared = GameFolder.documents.appendingPathComponent("horizon-download.zip")
        let previous = try? Data(contentsOf: shared)
        let sentinel = Data("keep-current-download".utf8)
        try sentinel.write(to: shared)
        defer { if let previous { try? previous.write(to: shared) } else { try? FileManager.default.removeItem(at: shared) } }
        let source = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: source) }
        try Data("stale-download".utf8).write(to: source)
        let imported = expectation(description: "Stale completion must not import")
        imported.isInverted = true
        let model = GameFilesDownload(importArchive: { _ in imported.fulfill(); return ["ok": true] })
        let session = URLSession(configuration: .ephemeral)
        defer { session.invalidateAndCancel() }
        let task = session.downloadTask(with: URL(string: "https://example.invalid/test.zip")!)
        model.urlSession(session, downloadTask: task, didFinishDownloadingTo: source)
        await fulfillment(of: [imported], timeout: 0.15)
        XCTAssertEqual(model.state, .idle)
        XCTAssertEqual(try Data(contentsOf: shared), sentinel)
    }
}

@MainActor
final class PackMutationTests: XCTestCase {
    func testLeavingThePackPageDiscardsAnInFlightResourceReport() async {
        let started = expectation(description: "resource report request started")
        var reply: CheckedContinuation<[String: Any], Never>?
        let choices = PackChoices(request: { command in
            XCTAssertEqual(command, "options-get resourcePacks")
            return await withCheckedContinuation {
                reply = $0
                started.fulfill()
            }
        })
        let load = Task { await choices.load() }
        await fulfillment(of: [started], timeout: 2)
        load.cancel()
        reply?.resume(returning: ["options": ["resourcePacks": ["value": ["file/stale.zip"]]]])
        await load.value

        XCTAssertTrue(choices.resourcePacks.isEmpty)
    }

    func testTimedOutPackDisableKeepsTheFileAndAllowsAnotherRefresh() async {
        let previous = Bridge.submit
        defer {
            Bridge.submit = previous
            DelayedStatusCallback.complete()
        }
        Bridge.submit = { _, context, done in
            MainActor.assumeIsolated { DelayedStatusCallback.store(context, done) }
        }
        var resources = ["file/fixture.zip"]
        let choices = PackChoices(request: { line in
            if line.hasPrefix("options-set ") {
                return await Bridge.send(line, responseTimeout: .milliseconds(20))
            }
            if line.hasPrefix("options-get ") {
                return ["options": ["resourcePacks": ["value": resources]]]
            }
            return ["selected": NSNull(), "packs": []]
        })
        let returned = expectation(description: "pack removal returned after the bridge timed out")
        let removal = Task {
            let error = await choices.remove("fixture.zip", kind: .resource) {
                XCTFail("A pack cannot be deleted without a confirmed disable")
                return nil
            }
            XCTAssertEqual(error, "The game did not confirm that the setting changed. Refresh and try again.")
            returned.fulfill()
        }
        await fulfillment(of: [returned], timeout: 1)
        XCTAssertTrue(DelayedStatusCallback.hasReply)
        DelayedStatusCallback.complete()
        await removal.value
        resources = ["file/new.zip"]
        await choices.load()
        XCTAssertEqual(choices.resourcePacks, resources)
        XCTAssertNil(choices.issue)
    }

    func testNewerShaderDownloadWinsEvenIfTheOlderOneFinishesLast() async throws {
        var selection: String?
        var mutations: [String] = []
        let request: PackChoices.Request = { command in
            if command.hasPrefix("shader-pack-set ") {
                let data = Data(command.dropFirst("shader-pack-set ".count).utf8)
                let args = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any]
                selection = args?["name"] as? String
                if let selection { mutations.append(selection) }
                return ["ok": true]
            }
            return ["selected": selection.map { $0 as Any } ?? NSNull()]
        }
        let first = PackChoices(request: request, packExists: { _, _ in true })
        let second = PackChoices(request: request, packExists: { _, _ in true })
        let oldIntent = first.beginShaderSelection(for: .shader)
        let newIntent = second.beginShaderSelection(for: .shader)

        let newResult = await second.enableDownloaded("new.zip", kind: .shader, shaderIntent: newIntent)
        let oldResult = await first.enableDownloaded("old.zip", kind: .shader, shaderIntent: oldIntent)

        XCTAssertTrue(try newResult.get())
        XCTAssertFalse(try oldResult.get())
        XCTAssertEqual(selection, "new.zip")
        XCTAssertEqual(mutations, ["new.zip"])
    }

    func testDeletingAnOldShaderDoesNotCancelActivationOfAnotherDownload() async throws {
        var selected: String? = "old.zip"
        var installed = Set(["old.zip", "new.zip"])
        var writes: [String] = []
        let choices = PackChoices(request: { command in
            let prefix = "shader-pack-set "
            guard command.hasPrefix(prefix) else {
                return ["selected": selected.map { $0 as Any } ?? NSNull()]
            }
            let data = Data(command.dropFirst(prefix.count).utf8)
            guard let mutation = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any],
                  let name = mutation["name"] as? String, let enabled = mutation["enabled"] as? Bool else {
                return ["error": "expected structured shader selection"]
            }
            selected = enabled ? name : nil
            writes.append("\(name):\(enabled)")
            return ["ok": true]
        }, packExists: { name, _ in installed.contains(name) })
        let download = choices.beginShaderSelection(for: .shader)

        let removal = await choices.remove("old.zip", kind: .shader) {
            installed.remove("old.zip")
            return nil
        }
        XCTAssertNil(removal)
        let activation = await choices.enableDownloaded("new.zip", kind: .shader, shaderIntent: download)
        XCTAssertTrue(try activation.get())
        XCTAssertEqual(selected, "new.zip")
        XCTAssertEqual(writes, ["old.zip:false", "new.zip:true"])
    }

    func testManualShaderSelectionSupersedesDownloadDuringItsStatusRead() async throws {
        let reading = expectation(description: "the finished download is checking the engine")
        var held: CheckedContinuation<[String: Any], Never>?
        var selection: String?
        var mutations: [String] = []
        var suspendNextRead = true
        let request: PackChoices.Request = { command in
            if command.hasPrefix("shader-pack-set ") {
                let data = Data(command.dropFirst("shader-pack-set ".count).utf8)
                let args = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any]
                selection = args?["name"] as? String
                if let selection { mutations.append(selection) }
                return ["ok": true]
            }
            if suspendNextRead {
                suspendNextRead = false
                return await withCheckedContinuation {
                    held = $0
                    reading.fulfill()
                }
            }
            return ["selected": selection.map { $0 as Any } ?? NSNull()]
        }
        let download = PackChoices(request: request, packExists: { _, _ in true })
        let manualChoices = PackChoices(request: request, packExists: { _, _ in true })
        let oldIntent = download.beginShaderSelection(for: .shader)
        let completed = Task {
            await download.enableDownloaded("downloaded.zip", kind: .shader, shaderIntent: oldIntent)
        }
        await fulfillment(of: [reading], timeout: 2)
        let manualIntent = manualChoices.beginShaderSelection(for: .shader)
        let manual = Task {
            await manualChoices.set("manual.zip", kind: .shader, on: true, shaderIntent: manualIntent)
        }
        held?.resume(returning: ["selected": NSNull()])

        let oldResult = await completed.value
        let manualError = await manual.value
        XCTAssertFalse(try oldResult.get())
        XCTAssertNil(manualError)
        XCTAssertEqual(mutations, ["manual.zip"])
        XCTAssertEqual(selection, "manual.zip")
    }

    func testPendingShaderDoesNotLookLikeACompletedRefusal() async {
        var runtime: [String: Any] = ["pending": true, "programs": [String](), "notes": ["Compiling"]]
        var resourceReads = 0
        var shaderReads = 0
        let choices = PackChoices(request: { line in
            if line == "shader-pack list" {
                shaderReads += 1
                return ["selected": "Pack.zip", "runtime": runtime]
            }
            resourceReads += 1
            return ["options": ["resourcePacks": ["value": [String]()]]]
        })
        await choices.load()
        XCTAssertEqual(choices.shaderStatus, "Preparing shader pack. Normal rendering is active.")
        XCTAssertEqual(choices.shaderNotes, ["Compiling"])
        XCTAssertTrue(choices.shaderPending)

        runtime = ["pending": false, "programs": [String](), "notes": ["Unsupported program"]]
        await choices.load()
        XCTAssertEqual(choices.shaderStatus, "No supported effects loaded. Normal rendering is active.")
        XCTAssertEqual(choices.shaderNotes, ["Unsupported program"])
        XCTAssertFalse(choices.shaderPending)
        XCTAssertEqual(resourceReads, 2)
        XCTAssertEqual(shaderReads, 2)
    }

    func testShaderStatusFollowsASettledPackThroughATransientFailure() async {
        var polls = 0
        let continued = expectation(description: "polling continues after the pack turns off")
        var pending: CheckedContinuation<[String: Any], Never>?
        let choices = PackChoices(request: { command in
            switch command {
            case "shader-pack list":
                return ["selected": "horizon:example", "packs": [["name": "Example.hshader", "key": "horizon:example"]],
                        "runtime": ["programs": ["final"]]]
            case "shader-pack-status":
                polls += 1
                switch polls {
                case 1: return ["error": "Connection interrupted"]
                case 2: return ["selected": "horizon:example", "runtime": ["programs": [String](), "notes": ["Resize failed"]]]
                case 3: return ["selected": NSNull(), "last_disabled": ["pack": "Example.hshader", "notes": ["Driver refused"]]]
                default:
                    return await withCheckedContinuation {
                        pending = $0
                        continued.fulfill()
                    }
                }
            default:
                return ["options": ["resourcePacks": ["value": [String]()]]]
            }
        })
        await choices.load()
        XCTAssertTrue(choices.isOn("Example.hshader", kind: .shader))
        XCTAssertFalse(choices.shaderPending)
        let follow = Task { await choices.followSelection(.shader, interval: .zero) }
        await fulfillment(of: [continued], timeout: 2)
        follow.cancel()
        pending?.resume(returning: ["selected": NSNull()])
        await follow.value
        XCTAssertEqual(polls, 4)
        XCTAssertNil(choices.issue)
        XCTAssertNil(choices.shaderPack)
        XCTAssertEqual(choices.shaderNotes, ["Driver refused"])
    }

    func testShaderPollingRecoversFromTheInitialReadFailure() async {
        var lists = 0
        var polls = 0
        let continued = expectation(description: "the next status poll follows the recovered pack")
        var pending: CheckedContinuation<[String: Any], Never>?
        let choices = PackChoices(request: { command in
            switch command {
            case "shader-pack list":
                lists += 1
                if lists == 1 { return ["error": "Initial status unavailable"] }
                return ["selected": "horizon:new", "packs": [["name": "New.hshader", "key": "horizon:new"]],
                        "runtime": ["programs": ["final"]]]
            case "shader-pack-status":
                polls += 1
                if polls == 1 { return ["selected": "horizon:new"] }
                return await withCheckedContinuation {
                    pending = $0
                    continued.fulfill()
                }
            default:
                return ["options": ["resourcePacks": ["value": [String]()]]]
            }
        })
        await choices.load()
        XCTAssertNil(choices.shaderPack)
        XCTAssertEqual(choices.issue, "Initial status unavailable")
        let follow = Task { await choices.followSelection(.shader, interval: .zero) }
        await fulfillment(of: [continued], timeout: 2)
        follow.cancel()
        pending?.resume(returning: ["selected": NSNull()])
        await follow.value
        XCTAssertEqual(lists, 2)
        XCTAssertTrue(choices.isOn("New.hshader", kind: .shader))
        XCTAssertNil(choices.issue)
    }

    func testLeavingTheShaderPageDiscardsAnInFlightReport() async {
        let requested = expectation(description: "shader status requested")
        var pending: CheckedContinuation<[String: Any], Never>?
        var seen = false
        let choices = PackChoices(request: { line in
            guard line == "shader-pack list" || line == "shader-pack-status" else {
                return ["options": ["resourcePacks": ["value": [String]()]]]
            }
            if !seen {
                seen = true
                return ["selected": "Pack.zip", "runtime": ["pending": true, "programs": [String]()]]
            }
            return await withCheckedContinuation {
                pending = $0
                requested.fulfill()
            }
        })
        await choices.load()
        let follow = Task { await choices.followSelection(.shader, interval: .zero) }
        await fulfillment(of: [requested], timeout: 2)
        follow.cancel()
        pending?.resume(returning: ["selected": NSNull()])
        await follow.value
        XCTAssertEqual(choices.shaderPack, "Pack.zip")
        XCTAssertTrue(choices.shaderPending)
    }

    func testResourceSelectionFollowsRollbackWhileThePackPageStaysOpen() async {
        var reads = 0
        let pack = "file/Failed.zip"
        let choices = PackChoices(request: { command in
            if command == "shader-pack list" { return ["selected": NSNull()] }
            XCTAssertEqual(command, "options-get resourcePacks")
            reads += 1
            switch reads {
            case 1: return ["options": ["resourcePacks": ["value": [pack]]]]
            case 2: return ["error": "Waiting for the game"]
            case 3: return ["options": ["resourcePacks": ["value": [String]()]]]
            default:
                withUnsafeCurrentTask { $0?.cancel() }
                return ["options": ["resourcePacks": ["value": [pack]]]]
            }
        })
        await choices.load()
        XCTAssertTrue(choices.isOn("Failed.zip", kind: .resource))
        let follow = Task { await choices.followSelection(.resource, interval: .zero) }
        await follow.value
        XCTAssertEqual(reads, 4)
        XCTAssertFalse(choices.isOn("Failed.zip", kind: .resource))
        XCTAssertNil(choices.issue)
    }

    func testAutomaticallyDisabledShaderShowsItsDiagnosticWithoutAppearingActive() async {
        var shader: [String: Any] = [
            "selected": NSNull(),
            "last_disabled": ["pack": "Failed.zip", "notes": ["Missing renderer input"]],
        ]
        let choices = PackChoices(request: { command in
            if command == "shader-pack list" { return shader }
            return ["options": ["resourcePacks": ["value": [String]()]]]
        })
        await choices.load()
        XCTAssertNil(choices.shaderPack)
        XCTAssertFalse(choices.isOn("Failed.zip", kind: .shader))
        XCTAssertEqual(choices.shaderNotes, ["Missing renderer input"])
        XCTAssertTrue(choices.shaderStatus?.contains("Failed.zip was turned off") == true)

        shader["selected"] = "Working.zip"
        shader["runtime"] = ["programs": ["composite"], "notes": ["Current note"]]
        await choices.load()
        XCTAssertEqual(choices.shaderPack, "Working.zip")
        XCTAssertEqual(choices.shaderStatus, "Running 1 shader programs.")
        XCTAssertEqual(choices.shaderNotes, ["Current note"])
    }

    func testInstalledListUsesTheGamePackLayouts() throws {
        let files = FileManager.default
        let root = files.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? files.removeItem(at: root) }

        func folder(_ kind: PackKind) -> URL { root.appendingPathComponent(kind.rawValue) }
        func makeDirectory(_ relative: String, in kind: PackKind) throws {
            try files.createDirectory(at: folder(kind).appendingPathComponent(relative),
                                      withIntermediateDirectories: true)
        }
        func makeFile(_ relative: String, in kind: PackKind) throws {
            try Data("fixture".utf8).write(to: folder(kind).appendingPathComponent(relative))
        }

        try makeDirectory("Valid resources", in: .resource)
        try makeFile("Valid resources/pack.mcmeta", in: .resource)
        try makeDirectory("Empty.zip", in: .resource)
        try makeDirectory("Wrong marker/pack.mcmeta", in: .resource)
        try makeFile("A.ZIP", in: .resource)
        try makeFile(".hidden.zip", in: .resource)

        try makeDirectory("Classic/shaders", in: .shader)
        try makeDirectory("Native", in: .shader)
        try makeFile("Native/horizon-shaders.toml", in: .shader)
        try makeDirectory("Empty.hshader", in: .shader)
        try makeDirectory("Bad native/horizon-shaders.toml", in: .shader)
        try makeDirectory("Bad classic", in: .shader)
        try makeFile("Bad classic/shaders", in: .shader)
        try makeFile("Bundled.HSHADER", in: .shader)

        let store = PackStore(folder: folder)
        store.reloadInstalled()

        XCTAssertEqual(store.installed[.resource], ["A.ZIP", "Valid resources"])
        XCTAssertEqual(store.installed[.shader], ["Bundled.HSHADER", "Classic", "Native"])
    }

    func testNativeShaderFileUsesTheManifestIdentityForTogglesAndRemoval() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let filename = "Example.hshader"
        let key = "horizon:example"
        let store = PackStore(folder: { root.appendingPathComponent($0.rawValue) })
        for kind in PackKind.allCases {
            let folder = root.appendingPathComponent(kind.rawValue)
            try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
            try Data().write(to: folder.appendingPathComponent(filename))
        }
        store.reloadInstalled()
        XCTAssertEqual(store.installed[.shader], [filename])
        XCTAssertEqual(store.installed[.resource], [])

        var selected: String? = key
        var writes: [Bool] = []
        let choices = PackChoices(request: { command in
            if command == "options-get resourcePacks" {
                return ["options": ["resourcePacks": ["value": [String]()]]]
            }
            if command == "shader-pack list" {
                return ["selected": selected.map { $0 as Any } ?? NSNull(),
                        "packs": [["name": filename, "key": key]]]
            }
            let prefix = "shader-pack-set "
            guard command.hasPrefix(prefix),
                  let data = (try? JSONSerialization.jsonObject(with: Data(command.dropFirst(prefix.count).utf8))) as? [String: Any],
                  data["name"] as? String == key, let enabled = data["enabled"] as? Bool else {
                return ["error": "expected native shader identity"]
            }
            writes.append(enabled)
            selected = enabled ? key : nil
            return ["ok": true]
        }, packExists: { name, kind in
            FileManager.default.fileExists(atPath: root.appendingPathComponent(kind.rawValue).appendingPathComponent(name).path)
        })
        await choices.load()
        XCTAssertTrue(choices.isOn(filename, kind: .shader))
        let off = await choices.set(filename, kind: .shader, on: false)
        XCTAssertNil(off)
        XCTAssertFalse(choices.isOn(filename, kind: .shader))
        let on = await choices.set(filename, kind: .shader, on: true)
        XCTAssertNil(on)
        XCTAssertTrue(choices.isOn(filename, kind: .shader))
        let removal = await choices.remove(filename, kind: .shader, delete: {
            XCTAssertNil(selected)
            return store.remove(filename, kind: .shader)
        })
        XCTAssertNil(removal)
        XCTAssertEqual(writes, [false, true, false])
        XCTAssertEqual(store.installed[.shader], [])
    }

    func testMissingNativeSelectionDoesNotPreventRemovingAnIdentifiedDifferentPack() async {
        var requests: [String] = []
        var removed: [String] = []
        let choices = PackChoices(request: { command in
            requests.append(command)
            return [
                "selected": "horizon:missing",
                "packs": [["name": "Classic.zip", "key": "Classic.zip"]]
            ]
        })
        let error = await choices.remove("Classic.zip", kind: .shader) {
            removed.append("Classic.zip")
            return nil
        }
        XCTAssertNil(error)
        XCTAssertEqual(removed, ["Classic.zip"])
        XCTAssertEqual(requests, ["shader-pack list"])
        XCTAssertEqual(choices.shaderPack, "horizon:missing")

        let unresolved = await choices.remove("Unlisted.hshader", kind: .shader) {
            removed.append("Unlisted.hshader")
            return nil
        }
        XCTAssertNotNil(unresolved)
        XCTAssertEqual(removed, ["Classic.zip"])
    }

    func testStaleShaderOffDoesNotDisableTheNewlySelectedPack() async {
        var current = "a.zip"
        var writes: [String] = []
        var ack: CheckedContinuation<Void, Never>?
        let submitted = expectation(description: "new shader activation submitted")
        let choices = PackChoices(request: { command in
            if command == "shader-pack list" { return ["selected": current] }
            let prefix = "shader-pack-set "
            guard command.hasPrefix(prefix),
                  let selection = (try? JSONSerialization.jsonObject(with: Data(command.dropFirst(prefix.count).utf8))) as? [String: Any],
                  let name = selection["name"] as? String, let enabled = selection["enabled"] as? Bool else {
                return ["error": "expected structured shader selection"]
            }
            writes.append("\(name):\(enabled)")
            if name == "b.zip" && enabled {
                await withCheckedContinuation {
                    ack = $0
                    submitted.fulfill()
                }
                current = "b.zip"
            } else {
                current = ""
            }
            return ["ok": true]
        }, packExists: { _, _ in true })
        let activate = Task { await choices.set("b.zip", kind: .shader, on: true) }
        await fulfillment(of: [submitted], timeout: 2)
        let staleOff = Task { await choices.set("a.zip", kind: .shader, on: false) }
        ack?.resume()
        let activationError = await activate.value
        let staleError = await staleOff.value
        XCTAssertNil(activationError)
        XCTAssertNil(staleError)
        XCTAssertEqual(writes, ["b.zip:true"])
        XCTAssertEqual(choices.shaderPack, "b.zip")
    }

    func testCancelledPackRemovalDuringStateReadDoesNotMutateOrDelete() async {
        var read: CheckedContinuation<[String: Any], Never>?
        var commands: [String] = []
        var deleted = false
        let reading = expectation(description: "selection read suspended")
        let choices = PackChoices(request: { command in
            commands.append(command)
            if commands.count != 1 { return ["selected": NSNull()] }
            return await withCheckedContinuation {
                read = $0
                reading.fulfill()
            }
        })
        let removal = Task { await choices.remove("a.zip", kind: .shader, delete: {
            deleted = true
            return nil
        }) }
        await fulfillment(of: [reading], timeout: 2)
        removal.cancel()
        read?.resume(returning: ["selected": "a.zip"])
        _ = await removal.value
        XCTAssertFalse(deleted)
        XCTAssertEqual(commands, ["shader-pack list"])
    }

    func testUnknownActiveNativeShaderDoesNotAuthorizeDeletion() async {
        let choices = PackChoices(request: { _ in
            ["selected": "horizon:aurora", "packs": [["name": "Other.hshader", "key": "horizon:other"]]]
        })
        var deleted = false
        let error = await choices.remove("Aurora.hshader", kind: .shader, delete: {
            deleted = true
            return nil
        })
        XCTAssertNotNil(error)
        XCTAssertFalse(deleted)
    }

    func testMalformedShaderSelectionDoesNotAuthorizeDeletion() async {
        var deleted = false
        let choices = PackChoices(request: { _ in ["selected": false] })
        let error = await choices.remove("a.zip", kind: .shader, delete: { deleted = true; return nil })
        XCTAssertNotNil(error)
        XCTAssertFalse(deleted)
    }

    func testSimultaneousResourceTogglesMergeAfterFirstAcknowledgement() async {
        var packs: [String] = []
        var writes: [[String]] = []
        var firstAck: CheckedContinuation<Void, Never>?
        let choices = PackChoices(request: { line in
            if line == "options-get resourcePacks" { return ["options": ["resourcePacks": ["value": packs]]] }
            if line.hasPrefix("options-set resourcePacks ") {
                let json = String(line.dropFirst("options-set resourcePacks ".count))
                let next = (try? JSONSerialization.jsonObject(with: Data(json.utf8))) as? [String] ?? []
                writes.append(next)
                if writes.count == 1 { await withCheckedContinuation { firstAck = $0 } }
                packs = next
                return ["ok": true]
            }
            return ["selected": NSNull()]
        }, packExists: { _, _ in true })
        let first = Task { await choices.set("a.zip", kind: .resource, on: true) }
        while firstAck == nil { await Task.yield() }
        let second = Task { await choices.set("b.zip", kind: .resource, on: true) }
        for _ in 0..<10 { await Task.yield() }
        XCTAssertEqual(writes.count, 1)
        firstAck?.resume()
        let firstError = await first.value; let secondError = await second.value
        XCTAssertNil(firstError); XCTAssertNil(secondError)
        XCTAssertEqual(writes, [["file/a.zip"], ["file/a.zip", "file/b.zip"]])
        XCTAssertEqual(choices.resourcePacks, ["file/a.zip", "file/b.zip"])
    }
    func testQueuedEnableDoesNotReactivateADeletedPack() async {
        let filename = "queued.zip"
        var packs = ["file/queued.zip"]
        var writes = 0
        var installed = true
        var firstWrite: CheckedContinuation<Void, Never>?
        let choices = PackChoices(request: { line in
            if line.hasPrefix("options-set resourcePacks ") {
                writes += 1
                if writes == 1 { await withCheckedContinuation { firstWrite = $0 } }
                let json = String(line.dropFirst("options-set resourcePacks ".count))
                packs = (try? JSONDecoder().decode([String].self, from: Data(json.utf8))) ?? []
                return ["ok": true]
            }
            return ["options": ["resourcePacks": ["value": packs]]]
        }, packExists: { _, _ in installed })
        let removal = Task {
            await choices.remove(filename, kind: .resource, delete: {
                installed = false
                return nil
            })
        }
        while firstWrite == nil { await Task.yield() }
        let enable = Task { await choices.set(filename, kind: .resource, on: true) }
        await Task.yield()
        firstWrite?.resume()
        let removalError = await removal.value
        let enableError = await enable.value
        XCTAssertNil(removalError)
        XCTAssertEqual(enableError, "That pack is no longer installed.")
        XCTAssertEqual(writes, 1)
        XCTAssertTrue(packs.isEmpty)
        XCTAssertFalse(choices.isOn(filename, kind: .resource))
    }

    func testAReadAfterAWriteTimeoutReconcilesTheToggleWithoutAuthorizingDeletion() async {
        var packs: [String] = []
        let writeError = "The game did not confirm that the setting changed."
        let choices = PackChoices(request: { command in
            let prefix = "options-set resourcePacks "
            if command.hasPrefix(prefix) {
                let json = Data(command.dropFirst(prefix.count).utf8)
                packs = (try? JSONDecoder().decode([String].self, from: json)) ?? []
                return ["error": writeError]
            }
            return ["options": ["resourcePacks": ["value": packs]]]
        }, packExists: { _, _ in true })

        let activation = await choices.set("a.zip", kind: .resource, on: true)
        XCTAssertNil(activation)
        XCTAssertTrue(choices.isOn("a.zip", kind: .resource))

        var deleted = false
        let removal = await choices.remove("a.zip", kind: .resource) {
            deleted = true
            return nil
        }
        XCTAssertEqual(removal, writeError)
        XCTAssertFalse(deleted)
        XCTAssertFalse(choices.isOn("a.zip", kind: .resource))
    }

    func testActivePackDisableFailureLeavesFileUntouched() async {
        var deleted = false
        let choices = PackChoices(request: { line in
            if line.hasPrefix("options-set") { return ["error": "Reload failed"] }
            return ["options": ["resourcePacks": ["value": ["file/a.zip"]]]]
        })
        let error = await choices.remove("a.zip", kind: .resource, delete: { deleted = true; return nil })
        XCTAssertEqual(error, "Reload failed")
        XCTAssertFalse(deleted)
        XCTAssertTrue(choices.isOn("a.zip", kind: .resource))
    }
    func testUnconfirmedDisableDoesNotDeleteFile() async {
        var deleted = false
        let choices = PackChoices(request: { line in
            if line.hasPrefix("options-set") { return ["ok": true] }
            return ["options": ["resourcePacks": ["value": ["file/a.zip"]]]]
        })
        let error = await choices.remove("a.zip", kind: .resource, delete: { deleted = true; return nil })
        XCTAssertNotNil(error)
        XCTAssertFalse(deleted)
    }
    func testDeletionRunsOnlyAfterConfirmedDisable() async {
        var packs = ["file/a.zip"]
        var events: [String] = []
        let choices = PackChoices(request: { line in
            if line.hasPrefix("options-set") { events.append("disable"); packs = []; return ["ok": true] }
            return ["options": ["resourcePacks": ["value": packs]]]
        })
        let error = await choices.remove("a.zip", kind: .resource, delete: { events.append("delete"); XCTAssertTrue(packs.isEmpty); return nil })
        XCTAssertNil(error)
        XCTAssertEqual(events, ["disable", "delete"])
    }
}

@MainActor
final class DownloadScreenTests: XCTestCase {
    func testStalledDownloadClearsSpeedAndETA() async {
        var now: TimeInterval = 0
        var received: Int64 = 0
        let model = DownloadModel(
            request: { _ in ["phase": "downloading", "received": received, "total": 10_000] },
            uptime: { now }
        )
        await model.refresh()
        received = 1_000
        for second in 1...5 {
            now = TimeInterval(second)
            await model.refresh()
            if second == 1 { XCTAssertNotNil(model.secondsLeft) }
        }
        XCTAssertEqual(model.speed, 0)
        XCTAssertNil(model.secondsLeft)
    }

    func testDownloadRestartIgnoresMismatchedCountersAndStartsMeasurementsOver() async {
        var now: TimeInterval = 0
        var received: Int64 = 100
        var total: Int64 = 10_000
        let model = DownloadModel(
            request: { _ in ["phase": "downloading", "received": received, "total": total] },
            uptime: { now }
        )
        await model.refresh()
        now = 1
        received = 2_000
        await model.refresh()
        XCTAssertNotNil(model.secondsLeft)

        total = 1_500
        await model.refresh()
        XCTAssertEqual(model.total, 10_000)
        XCTAssertEqual(model.received, 2_000)

        received = 50
        await model.refresh()
        XCTAssertEqual(model.total, 1_500)
        XCTAssertEqual(model.received, 50)
        XCTAssertEqual(model.speed, 0)
        XCTAssertNil(model.secondsLeft)
    }

    func testDownloadSpeedStartsFreshAfterPollingPauses() async {
        var now: TimeInterval = 10
        var received: Int64 = 100
        let model = DownloadModel(
            request: { _ in ["phase": "downloading", "received": received, "total": 10_000] },
            uptime: { now }
        )
        await model.refresh()
        now = 11
        received = 1_100
        await model.refresh()
        XCTAssertEqual(model.speed, 1_000)
        XCTAssertNotNil(model.secondsLeft)

        now = 17
        received = 2_100
        await model.refresh()
        XCTAssertEqual(model.speed, 0)
        XCTAssertNil(model.secondsLeft)

        now = 18
        received = 4_100
        await model.refresh()
        XCTAssertEqual(model.speed, 2_000)
        XCTAssertEqual(model.secondsLeft, 2.95)
    }

    func testBridgeFailureShowsRecoveryActionsAndIsLoggedOnce() async {
        var messages: [String] = []
        let model = DownloadModel(request: { _ in ["error": "Game connection unavailable"] },
                                  diagnostic: { messages.append($0) })
        await model.refresh()
        XCTAssertEqual(model.phase, .failed("Game connection unavailable"))
        await model.refresh()
        XCTAssertEqual(messages.count, 1)
    }

    func testRetryErrorSurvivesOldFailurePollsUntilTheTransferResumes() async {
        var state: [String: Any] = ["phase": "failed", "error": "Connection dropped"]
        let model = DownloadModel(request: { command in
            command == "download-retry" ? ["error": "Could not open download storage"] : state
        }, diagnostic: { _ in })
        await model.refresh()
        await model.retry()
        await model.refresh()
        XCTAssertEqual(model.phase, .failed("Could not open download storage"))

        state = ["phase": "downloading", "received": 20, "total": 50]
        await model.refresh()
        XCTAssertEqual(model.phase, .downloading)
        state = ["phase": "failed", "error": "Connection lost again"]
        await model.refresh()
        XCTAssertEqual(model.phase, .failed("Connection lost again"))
    }

    func testRetryWithoutAnAcknowledgmentShowsAnError() async {
        let model = DownloadModel(request: { command in
            command == "download-retry" ? [:] : ["phase": "failed", "error": "Connection dropped"]
        }, diagnostic: { _ in })
        await model.refresh()
        await model.retry()
        XCTAssertEqual(model.phase, .failed("Horizon could not confirm that the download restarted."))
    }

    func testRetryDoesNotAcceptANumericAcknowledgment() async {
        let model = DownloadModel(request: { command in
            command == "download-retry" ? ["ok": 1] : ["phase": "failed", "error": "Connection dropped"]
        }, diagnostic: { _ in })
        await model.refresh()
        await model.retry()
        XCTAssertEqual(model.phase, .failed("Horizon could not confirm that the download restarted."))
    }

    func testPreRetryPollCannotReplaceTheCurrentDownloadState() async {
        var oldPoll: CheckedContinuation<[String: Any], Never>?
        let polling = expectation(description: "old status request is in flight")
        var polls = 0
        let model = DownloadModel(request: { command in
            if command == "download-retry" { return ["ok": true] }
            polls += 1
            switch polls {
            case 1: return ["phase": "failed", "error": "Old download failed"]
            case 2: return await withCheckedContinuation {
                oldPoll = $0
                polling.fulfill()
            }
            default: return ["phase": "downloading", "received": 20, "total": 50]
            }
        }, diagnostic: { _ in })
        await model.refresh()
        let stale = Task { await model.refresh() }
        await fulfillment(of: [polling], timeout: 2)
        await model.retry()
        await model.refresh()
        oldPoll?.resume(returning: ["phase": "failed", "error": "Old download failed"])
        await stale.value
        XCTAssertEqual(model.phase, .downloading)
        XCTAssertEqual(model.received, 20)
        XCTAssertEqual(model.total, 50)
    }
}

@MainActor
final class AutomaticGameFilesTests: XCTestCase {
    func testPackTransferChecksSizeWithAndWithoutAContentLength() async throws {
        guard let url = ProcessInfo.processInfo.environment["HORIZON_DOWNLOAD_FIXTURE_URL"] else {
            throw XCTSkip("Requires the loopback download fixture")
        }
        let knownLength = URLRequest(url: try XCTUnwrap(URL(string: url)))
        let unknownLength = URLRequest(url: try XCTUnwrap(URL(string: url + "?without_length=1")))
        let expected = Data("horizon-download-fixture".utf8)
        for (request, advertisedSize) in [
            (knownLength, 3), (unknownLength, 3), (unknownLength, expected.count + 1),
        ] {
            do {
                let (file, _) = try await PackStore.transferArchive(request, expectedSize: Int64(advertisedSize))
                try? FileManager.default.removeItem(at: file)
                XCTFail("A response with the wrong number of bytes was accepted")
            } catch let error as PackError {
                XCTAssertTrue(error.message.contains("checksum or size"))
            }
        }
        let (file, _) = try await PackStore.transferArchive(unknownLength, expectedSize: Int64(expected.count))
        defer { try? FileManager.default.removeItem(at: file) }
        XCTAssertEqual(try Data(contentsOf: file), expected)
    }

    func testChangingDownloadOriginClearsThePreviousToken() {
        let model = GameFilesDownload(diagnostic: { _ in })
        for link in [
            "https://other.example.invalid/archive",
            "https://files.example.invalid:444/archive",
            "http://files.example.invalid/archive",
            "",
        ] {
            model.link = "https://files.example.invalid/initial"
            model.token = "fixture"
            model.link = "https://FILES.example.invalid:443/new-file"
            XCTAssertEqual(model.token, "fixture")
            model.link = link
            XCTAssertTrue(model.token.isEmpty, link)
        }
    }

    func testSavedCredentialCanOnlyBeReadForItsStoredOrigin() throws {
        let origin = try XCTUnwrap(GameFilesOrigin(URL(string: "https://files.example.invalid/archive")))
        let data = try JSONEncoder().encode(GameFilesCredential(origin: origin, token: "fixture"))
        XCTAssertEqual(GameFilesCredential.token(from: data, for: origin), "fixture")
        for link in [
            "http://files.example.invalid/archive",
            "https://files.example.invalid:444/archive",
            "https://other.example.invalid/archive",
        ] {
            XCTAssertNil(GameFilesCredential.token(from: data, for: GameFilesOrigin(URL(string: link))), link)
        }
        XCTAssertNil(GameFilesCredential.token(from: Data("legacy-token".utf8), for: origin))
    }

    func testCustomDownloadRejectsBearerTokenOverHTTP() {
        let model = GameFilesDownload(diagnostic: { _ in })
        model.link = "http://127.0.0.1:1/files.zip"
        model.token = "private-test-token"
        model.start()
        XCTAssertEqual(model.state, .failed("Use an HTTPS link to download with an access token."))
        XCTAssertFalse(model.busy)
    }

    func testRedirectKeepsHTTPSAndOnlySharesAuthorizationOnTheSameOrigin() throws {
        let https = URL(string: "https://files.example.invalid/archive")!
        let lan = URL(string: "http://192.168.1.2/archive")!
        for (source, url, keepsAuthorization) in [
            (https, "https://files.example.invalid:443/redirect", true),
            (https, "https://files.example.invalid:444/redirect", false),
            (https, "https://cdn.example.invalid/redirect", false),
            (lan, "http://192.168.1.2/redirect", false),
            (lan, "https://files.example.invalid/redirect", false),
        ] {
            var redirect = URLRequest(url: URL(string: url)!)
            redirect.setValue("Bearer fixture", forHTTPHeaderField: "Authorization")
            let safe = try XCTUnwrap(GameFilesDownload.redirectRequest(redirect, from: source))
            XCTAssertEqual(safe.value(forHTTPHeaderField: "Authorization"),
                           keepsAuthorization ? "Bearer fixture" : nil, url)
            XCTAssertEqual(safe.url, redirect.url)
        }
        let downgrade = URLRequest(url: URL(string: "http://files.example.invalid/redirect")!)
        XCTAssertNil(GameFilesDownload.redirectRequest(downgrade, from: https))
    }

    func testDownloadErrorFactsKeepCodesButNeverIncludeCredentials() {
        let underlying = NSError(domain:NSPOSIXErrorDomain,code:61,userInfo:[NSLocalizedDescriptionKey:"private-password"])
        let error = NSError(domain:NSURLErrorDomain,code:NSURLErrorCannotConnectToHost,userInfo:[
            NSUnderlyingErrorKey:underlying,
            NSURLErrorFailingURLErrorKey:URL(string:"https://user:pass@example.invalid/file?access_token=private-value")!,
            NSLocalizedDescriptionKey:"private-password"
        ])
        let facts = GameFilesDownload.errorFacts(error)
        XCTAssertTrue(facts.contains("kind=connection"))
        XCTAssertTrue(facts.contains("code=-1004"))
        XCTAssertTrue(facts.contains("underlying_code=61"))
        for sensitive in ["private", "example.invalid", "https", "user:pass"] {XCTAssertFalse(facts.contains(sensitive))}
    }

    func testFailedTransferLogsItsStageAndSafeNativeError() async {
        let restore = temporarilyRemoveResourceMarker()
        defer {restore()}
        var events: [String] = []
        let model = GameFilesDownload(automaticSource:{["url":"http://127.0.0.1:1/fixture?access_token=never-log-this"]},diagnostic:{events.append($0)})
        await model.startAutomatic()
        let deadline = Date().addingTimeInterval(10)
        while model.busy, Date() < deadline {try? await Task.sleep(nanoseconds:20_000_000)}
        XCTAssertFalse(model.busy)
        XCTAssertTrue(events.contains(where:{$0.contains("source_lookup_result has_url=true has_error=false")}))
        XCTAssertTrue(events.contains(where:{$0.contains("transfer_start endpoint=custom")}))
        XCTAssertTrue(events.contains(where:{$0.contains("transfer_failed") && $0.contains("domain=NSURLErrorDomain")}))
        XCTAssertFalse(events.joined().contains("never-log-this"))
        XCTAssertFalse(events.joined().contains("127.0.0.1"))
    }

    func testAutomaticSourceFailureNeedsExplicitRetry() async {
        let restore = temporarilyRemoveResourceMarker()
        defer { restore() }
        var calls = 0
        var events: [String] = []
        let model = GameFilesDownload(automaticSource: {
            calls += 1
            return calls == 1
                ? ["error": "Fixture source is unavailable", "url": "https://example.invalid/stale.zip"]
                : ["error": "Fixture source is unavailable"]
        }, diagnostic: {events.append($0)})
        model.link = "https://example.invalid/private.zip"
        model.token = "private-test-token"
        await model.startAutomatic()
        XCTAssertEqual(model.state, .failed("Fixture source is unavailable"))
        await model.startAutomaticallyIfNeeded()
        XCTAssertEqual(calls, 1)
        await model.startAutomatic()
        XCTAssertEqual(calls, 2)
        XCTAssertFalse(model.busy)
        XCTAssertEqual(model.link, "https://example.invalid/private.zip")
        XCTAssertEqual(model.token, "private-test-token")
        XCTAssertTrue(events.contains(where:{$0.contains("source_lookup_failed")}))
        XCTAssertFalse(events.contains(where:{$0.contains("transfer_start")}))
        XCTAssertFalse(events.joined().contains("private-test-token"))
    }

    func testCancelWhileResolvingSourceCannotStartLateDownload() async {
        var resume: CheckedContinuation<[String: Any], Never>?
        let model = GameFilesDownload(automaticSource: {
            let answer = await withCheckedContinuation { resume = $0 }
            XCTAssertTrue(Task.isCancelled)
            return answer
        })
        let start = Task { await model.startAutomatic() }
        while resume == nil { await Task.yield() }
        XCTAssertEqual(model.state, .findingSource)
        model.cancel()
        resume?.resume(returning: ["url": "https://example.invalid/must-not-fetch.zip"])
        await start.value
        XCTAssertEqual(model.state, .idle)
        XCTAssertFalse(model.busy)
    }

    func testStaleBackgroundExpirationDoesNotEndTheRetry() async throws {
        let restore = temporarilyRemoveResourceMarker()
        defer { restore() }
        let url = try XCTUnwrap(ProcessInfo.processInfo.environment["HORIZON_DOWNLOAD_FIXTURE_URL"]
                               ?? "http://127.0.0.1:1/game.zip")
        var expirations: [@MainActor @Sendable () -> Void] = []
        var ended: [UIBackgroundTaskIdentifier] = []
        var events: [String] = []
        let first = UIBackgroundTaskIdentifier(rawValue: 1)
        let second = UIBackgroundTaskIdentifier(rawValue: 2)
        let model = GameFilesDownload(
            automaticSource: { ["url": url + "?delay=1"] },
            diagnostic: { events.append($0) },
            backgroundTasks: GameFilesBackgroundTasks(begin: { expire in
                expirations.append(expire)
                return expirations.count == 1 ? first : second
            }, end: { ended.append($0) })
        )
        defer { model.cancel() }

        await model.startAutomatic()
        model.cancel()
        await model.startAutomatic()
        XCTAssertEqual(expirations.count, 2)
        guard expirations.count == 2 else { return }
        expirations[0]()
        XCTAssertEqual(ended, [first])
        XCTAssertFalse(events.contains { $0.contains("background_time_expired") })
        expirations[1]()
        expirations[1]()
        XCTAssertEqual(ended, [first, second])
        XCTAssertEqual(events.filter { $0.contains("background_time_expired") }.count, 1)
        model.cancel()
        XCTAssertEqual(ended, [first, second])
    }

    func testCancelledSetupTaskCannotStartLateDownloadAndCanRestart() async {
        let restore = temporarilyRemoveResourceMarker()
        defer { restore() }
        var resume: CheckedContinuation<[String: Any], Never>?
        var lookups = 0
        let model = GameFilesDownload(automaticSource: {
            lookups += 1
            if lookups > 1 { return ["error": "Fixture source unavailable"] }
            let answer = await withCheckedContinuation { resume = $0 }
            XCTAssertTrue(Task.isCancelled)
            return answer
        })
        let start = Task { await model.startAutomaticallyIfNeeded() }
        while resume == nil { await Task.yield() }
        start.cancel()
        resume?.resume(returning: ["url": "http://127.0.0.1:1/must-not-fetch.zip"])
        await start.value
        XCTAssertEqual(model.state, .idle)
        XCTAssertFalse(model.busy)
        await model.startAutomaticallyIfNeeded()
        XCTAssertEqual(lookups, 2)
        XCTAssertEqual(model.state, .failed("Fixture source unavailable"))
    }

    func testFilesArrivingDuringSourceLookupDoNotGetReplaced() async throws {
        let marker = GameFolder.resources.appendingPathComponent("horizon-resources.json")
        let previous = try? Data(contentsOf: marker)
        try? FileManager.default.removeItem(at: marker)
        defer {
            if let previous { try? previous.write(to: marker) }
            else { try? FileManager.default.removeItem(at: marker) }
        }
        var resume: CheckedContinuation<[String: Any], Never>?
        let model = GameFilesDownload(automaticSource: {
            await withCheckedContinuation { resume = $0 }
        })
        let start = Task { await model.startAutomaticallyIfNeeded() }
        while resume == nil { await Task.yield() }
        try FileManager.default.createDirectory(at: GameFolder.resources, withIntermediateDirectories: true)
        try Data("manual-import-fixture".utf8).write(to: marker)
        resume?.resume(returning: ["url": "http://127.0.0.1:1/must-not-fetch.zip"])
        await start.value
        XCTAssertEqual(model.state, .done("Game files are already installed."))
        XCTAssertEqual(try Data(contentsOf: marker), Data("manual-import-fixture".utf8))
        XCTAssertFalse(model.busy)
    }

    func testAlreadyInstalledFilesDoNotDownload() async throws {
        let marker = GameFolder.resources.appendingPathComponent("horizon-resources.json")
        let previous = try? Data(contentsOf: marker)
        try FileManager.default.createDirectory(at: GameFolder.resources, withIntermediateDirectories: true)
        try Data("{}".utf8).write(to: marker)
        defer {
            if let previous { try? previous.write(to: marker) }
            else { try? FileManager.default.removeItem(at: marker) }
        }
        var calls = 0
        let model = GameFilesDownload(automaticSource: { calls += 1; return [:] })
        await model.startAutomaticallyIfNeeded()
        XCTAssertEqual(calls, 0)
        XCTAssertEqual(model.state, .idle)
    }

    func testCancelledLocalImportDoesNotLeaveSetupBusy() async throws {
        let file = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString + ".zip")
        try Data("local-import-fixture".utf8).write(to: file)
        defer { try? FileManager.default.removeItem(at: file) }
        var imports = 0
        let model = GameFilesDownload(importArchive: { _ in
            imports += 1
            return ["ok": true]
        }, diagnostic: { message in
            if message.contains("file_import_start") {
                withUnsafeCurrentTask { $0?.cancel() }
            }
        })
        let operation = Task { await model.importPreparedFile(file) }
        await operation.value
        XCTAssertEqual(imports, 0)
        XCTAssertEqual(model.state, .idle)
        XCTAssertFalse(model.busy, "Cancelled file hydration must allow a retry")
    }

    func testCancelButtonStopsArchivePreparationBeforeEngineImport() async throws {
        let file = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString + ".zip")
        try Data("local-import-fixture".utf8).write(to: file)
        defer { try? FileManager.default.removeItem(at: file) }
        var imports = 0
        var observedCopy = false
        weak var observedModel: GameFilesDownload?
        let model = GameFilesDownload(importArchive: { _ in
            imports += 1
            return ["error": "Unexpected import"]
        }, diagnostic: { message in
            if message.contains("file_copy_start"), let model = observedModel {
                observedCopy = true
                XCTAssertEqual(model.state, .preparingArchive)
                XCTAssertTrue(model.canCancel)
                model.cancel()
                XCTAssertEqual(model.state, .cancellingArchive)
                XCTAssertTrue(model.busy)
                XCTAssertFalse(model.canCancel)
            }
        })
        observedModel = model

        await model.importPreparedFile(file)
        XCTAssertTrue(observedCopy)
        XCTAssertEqual(imports, 0)
        XCTAssertEqual(model.state, .idle)
        XCTAssertFalse(model.busy)
        XCTAssertEqual(try Data(contentsOf: file), Data("local-import-fixture".utf8))
    }

    func testCancellingInstallationDoesNotAllowAnOverlappingImport() async throws {
        let restore = temporarilyRemoveResourceMarker()
        defer { restore() }
        let file = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString + ".zip")
        try Data("local-import-fixture".utf8).write(to: file)
        defer { try? FileManager.default.removeItem(at: file) }
        let installing = expectation(description: "Importer is holding the staged archive")
        var complete: CheckedContinuation<[String: Any], Never>?
        var imports = 0
        let model = GameFilesDownload(importArchive: { _ in
            imports += 1
            guard imports == 1 else { return ["error": "Unexpected concurrent import"] }
            return await withCheckedContinuation {
                complete = $0
                installing.fulfill()
            }
        })
        let first = Task { await model.importPreparedFile(file) }
        await fulfillment(of: [installing], timeout: 5)
        guard let complete else {
            first.cancel()
            return
        }
        XCTAssertEqual(model.state, .installing)
        XCTAssertFalse(model.canCancel)
        model.cancel()
        XCTAssertEqual(model.state, .installing)
        await model.importPreparedFile(file)
        XCTAssertEqual(imports, 1)

        try installFixtureResourceMarker()
        complete.resume(returning: ["ok": true, "kind": "resources"])
        await first.value
        XCTAssertFalse(model.busy)
        if case .done = model.state {} else { XCTFail("Expected completed install, got \(model.state)") }
    }

    func testAutomaticDownloadUsesResourceImporterAndKeepsCustomCredentials() async throws {
        let restore = temporarilyRemoveResourceMarker()
        defer { restore() }
        guard let url = ProcessInfo.processInfo.environment["HORIZON_DOWNLOAD_FIXTURE_URL"] else {
            throw XCTSkip("Requires the loopback download fixture")
        }
        let imported = expectation(description: "Downloaded fixture reaches importer")
        let model = GameFilesDownload(importArchive: { line in
            XCTAssertTrue(line.hasPrefix("import-game-files "))
            let payload = Data(line.dropFirst("import-game-files ".count).utf8)
            let request = try? JSONDecoder().decode([String: String].self, from: payload)
            XCTAssertEqual(request?["resource_marker"], "missing")
            let file = URL(fileURLWithPath: request?["path"] ?? "")
            XCTAssertEqual(try? Data(contentsOf: file), Data("horizon-download-fixture".utf8))
            do { try installFixtureResourceMarker() }
            catch {
                XCTFail("Could not install fixture marker: \(error)")
                return ["error": "Fixture marker was not installed"]
            }
            imported.fulfill()
            return ["ok": true, "kind": "resources"]
        }, automaticSource: { ["url": url] })
        model.link = "https://example.invalid/private.zip"
        model.token = "private-test-token"
        await model.startAutomatic()
        await fulfillment(of: [imported], timeout: 10)
        if case .done = model.state {} else { XCTFail("Download did not finish: \(model.state)") }
        XCTAssertEqual(model.link, "https://example.invalid/private.zip")
        XCTAssertEqual(model.token, "private-test-token")
    }

    func testDownloadRestartsOnceAfterTheConnectionDrops() async throws {
        let restore = temporarilyRemoveResourceMarker()
        defer { restore() }
        guard let url = ProcessInfo.processInfo.environment["HORIZON_DOWNLOAD_FIXTURE_URL"] else {
            throw XCTSkip("Requires the loopback download fixture")
        }
        let marker = GameFolder.resources.appendingPathComponent("horizon-resources.json")
        var imports = 0
        var events: [String] = []
        let model = GameFilesDownload(
            importArchive: { _ in
                imports += 1
                try? FileManager.default.createDirectory(at: GameFolder.resources, withIntermediateDirectories: true)
                try? Data("installed fixture".utf8).write(to: marker)
                return ["ok": true, "kind": "resources"]
            },
            automaticSource: { ["url": url + "?cut_once=" + UUID().uuidString] },
            diagnostic: { events.append($0) }
        )
        defer { model.cancel() }
        await model.startAutomatic()
        for _ in 0..<200 where model.busy {
            try await Task.sleep(for: .milliseconds(50))
        }
        XCTAssertEqual(imports, 1)
        XCTAssertEqual(events.filter { $0.contains("transfer_restarted reason=connection_lost") }.count, 1)
        if case .done = model.state {} else { XCTFail("Retry did not finish: \(model.state)") }
    }

    func testIncompleteHTTPResponsesDoNotReachTheImporter() async throws {
        let restore = temporarilyRemoveResourceMarker()
        defer { restore() }
        guard let url = ProcessInfo.processInfo.environment["HORIZON_DOWNLOAD_FIXTURE_URL"] else {
            throw XCTSkip("Requires the loopback download fixture")
        }
        for (query, reason) in [
            ("partial=1", "only part of the archive"),
            ("empty=1", "did not send an archive"),
            ("empty_ok=1", "did not send an archive"),
            ("truncated=1", nil),
            ("oversized=1", "2 GiB"),
        ] {
            var imports = 0
            let model = GameFilesDownload(importArchive: { _ in
                imports += 1
                return ["ok": true, "kind": "resources"]
            }, automaticSource: { ["url": url + "?" + query] })
            defer { model.cancel() }
            await model.startAutomatic()
            let deadline = Date().addingTimeInterval(10)
            while model.busy, Date() < deadline {
                try await Task.sleep(for: .milliseconds(20))
            }
            XCTAssertEqual(imports, 0, query)
            guard case let .failed(message) = model.state else {
                XCTFail("\(query): got \(model.state)")
                continue
            }
            if let reason { XCTAssertTrue(message.contains(reason), message) }
        }
    }

    func testRejectedFilesCanBeReinstalledFromTheLauncher() async throws {
        guard let url = ProcessInfo.processInfo.environment["HORIZON_DOWNLOAD_FIXTURE_URL"] else {
            throw XCTSkip("Requires the loopback download fixture")
        }
        let restore = temporarilyRemoveResourceMarker()
        defer { restore() }
        let marker = GameFolder.resources.appendingPathComponent("horizon-resources.json")
        try FileManager.default.createDirectory(at: GameFolder.resources, withIntermediateDirectories: true)
        try Data("rejected fixture".utf8).write(to: marker)
        let link = GameLink(send: { _ in ["phase": "needs_files", "reason": "Damaged tables"] })
        await link.refresh()
        XCTAssertTrue(link.needsGameFiles)
        XCTAssertTrue(link.repairingGameFiles)
        let rejected = try XCTUnwrap(GameFolder.resourceMarker(at: GameFolder.resources))

        let model = GameFilesDownload(importArchive: { line in
            XCTAssertTrue(line.hasPrefix("import-game-files "))
            let payload = Data(line.dropFirst("import-game-files ".count).utf8)
            let request = try? JSONDecoder().decode([String: String].self, from: payload)
            XCTAssertEqual(request?["resource_marker"], rejected)
            XCTAssertEqual(try? Data(contentsOf: marker), Data("rejected fixture".utf8))
            try? Data("installed fixture".utf8).write(to: marker, options: .atomic)
            return ["ok": true, "kind": "resources"]
        }, automaticSource: { ["url": url] })
        await model.startAutomatic(replacingRejectedFiles: link.repairingGameFiles)
        for _ in 0..<100 where model.busy {
            try await Task.sleep(for: .milliseconds(50))
        }
        XCTAssertEqual(try Data(contentsOf: marker), Data("installed fixture".utf8))
        if case .done = model.state {} else { XCTFail("Reinstall did not finish: \(model.state)") }
    }

    func testReinstallKeepsFilesReplacedDuringTransfer() async throws {
        guard let url = ProcessInfo.processInfo.environment["HORIZON_DOWNLOAD_FIXTURE_URL"] else {
            throw XCTSkip("Requires the loopback download fixture")
        }
        let restore = temporarilyRemoveResourceMarker()
        defer { restore() }
        let marker = GameFolder.resources.appendingPathComponent("horizon-resources.json")
        try FileManager.default.createDirectory(at: GameFolder.resources, withIntermediateDirectories: true)
        try Data("old".utf8).write(to: marker)
        let modified = try FileManager.default.attributesOfItem(atPath: marker.path)[.modificationDate]
        var imports = 0
        let model = GameFilesDownload(importArchive: { _ in
            imports += 1
            return ["ok": true, "kind": "resources"]
        }, automaticSource: { ["url": url + "?delay=1"] })
        await model.startAutomatic(replacingRejectedFiles: true)
        try Data("new".utf8).write(to: marker, options: .atomic)
        if let modified {
            try FileManager.default.setAttributes([.modificationDate: modified], ofItemAtPath: marker.path)
        }
        for _ in 0..<100 where model.busy {
            try await Task.sleep(for: .milliseconds(50))
        }
        XCTAssertEqual(imports, 0)
        XCTAssertEqual(model.state, .done("Game files are already installed."))
        XCTAssertEqual(try Data(contentsOf: marker), Data("new".utf8))
    }

    func testFilesArrivingDuringTransferDoNotGetReplaced() async throws {
        let restore = temporarilyRemoveResourceMarker()
        defer { restore() }
        guard let url = ProcessInfo.processInfo.environment["HORIZON_DOWNLOAD_FIXTURE_URL"] else {
            throw XCTSkip("Requires the loopback download fixture")
        }
        var imports = 0
        let model = GameFilesDownload(importArchive: { _ in
            imports += 1
            return ["ok": true, "kind": "resources"]
        }, automaticSource: { ["url": url + "?delay=1"] })
        await model.startAutomatic()
        let marker = GameFolder.resources.appendingPathComponent("horizon-resources.json")
        try FileManager.default.createDirectory(at: GameFolder.resources, withIntermediateDirectories: true)
        try Data("manual-transfer-fixture".utf8).write(to: marker)
        for _ in 0..<100 where model.busy {
            try await Task.sleep(for: .milliseconds(50))
        }
        XCTAssertEqual(imports, 0)
        XCTAssertEqual(model.state, .done("Game files are already installed."))
        XCTAssertEqual(try Data(contentsOf: marker), Data("manual-transfer-fixture".utf8))
    }
}


@MainActor
final class SkinHeadTests: XCTestCase {
    private func image(_ color: UIColor) -> UIImage {
        UIGraphicsImageRenderer(size: CGSize(width: 1, height: 1)).image { context in
            color.setFill()
            context.fill(CGRect(x: 0, y: 0, width: 1, height: 1))
        }
    }

    func testOldLookupCannotReplaceNewAccountFace() async {
        let oldFace = image(.red), newFace = image(.blue)
        var pending: [String: CheckedContinuation<UIImage?, Never>] = [:]
        let head = SkinHead { name in
            await withCheckedContinuation { pending[name] = $0 }
        }
        let old = Task { await head.load("Old") }
        while pending["Old"] == nil { await Task.yield() }
        let current = Task { await head.load("Current") }
        while pending["Current"] == nil { await Task.yield() }
        pending.removeValue(forKey: "Current")?.resume(returning: newFace)
        await current.value
        XCTAssertTrue(head.face === newFace)
        pending.removeValue(forKey: "Old")?.resume(returning: oldFace)
        await old.value
        XCTAssertTrue(head.face === newFace, "A previous account lookup must not replace the selected account")
    }

    func testCachedAndClearedSelectionInvalidatePendingLookup() async {
        let cached = image(.green), oldFace = image(.red)
        var pending: CheckedContinuation<UIImage?, Never>?
        let head = SkinHead { name in
            if name == "Cached" { return cached }
            return await withCheckedContinuation { pending = $0 }
        }
        await head.load("Cached")
        let old = Task { await head.load("Old") }
        while pending == nil { await Task.yield() }
        await head.load("Cached")
        pending?.resume(returning: oldFace); pending = nil
        await old.value
        XCTAssertTrue(head.face === cached)

        let other = Task { await head.load("Other") }
        while pending == nil { await Task.yield() }
        await head.load(nil)
        pending?.resume(returning: oldFace); pending = nil
        await other.value
        XCTAssertNil(head.face)
    }

    func testAlreadyCancelledLookupCannotClearCurrentSelection() async {
        let current = image(.blue)
        var calls = 0
        let head = SkinHead { _ in calls += 1; return current }
        await head.load("Current")

        let stale = Task { await head.load(nil) }
        stale.cancel()
        await stale.value
        XCTAssertTrue(head.face === current)

        let cancelledFetch = Task { await head.load("Previous") }
        cancelledFetch.cancel()
        await cancelledFetch.value
        XCTAssertEqual(calls, 1)
        XCTAssertTrue(head.face === current)
    }

    func testCancelledLookupDoesNotPublishFace() async {
        let face = image(.red)
        var pending: CheckedContinuation<UIImage?, Never>?
        let head = SkinHead { _ in
            await withCheckedContinuation { pending = $0 }
        }
        let load = Task { await head.load("Cancelled") }
        while pending == nil { await Task.yield() }
        load.cancel(); pending?.resume(returning: face)
        await load.value
        XCTAssertNil(head.face)
    }
}
