import AVFoundation
import UIKit

/// Owns the game's RemoteIO audio session. Serialize changes off the UI thread so a slow
/// system audio transition cannot stall Play, Quit, or a notification callback.
final class GameAudio: @unchecked Sendable {
    struct ResumptionEvent: Sendable {
        enum Recommendation: Sendable { case ignore, resume, stayInactive }

        let name: Notification.Name
        let recommendation: @Sendable (Notification) -> Recommendation
        let interruptionName: Notification.Name
        let isInterruption: @Sendable (Notification) -> Bool

        static var system: Self {
            #if compiler(>=6.4)
            if #available(iOS 27.0, *) {
                return Self(
                    name: AVAudioSession.resumptionRecommendationNotification,
                    recommendation: { note in
                        let context = note.userInfo?[AVAudioSession.resumptionContextKey]
                            as? AVAudioSession.ResumptionContext
                        switch context?.recommendation {
                        case .shouldResume: return .resume
                        case .shouldNotResume: return .stayInactive
                        default: return .ignore
                        }
                    },
                    interruptionName: AVAudioSession.didBecomeInactiveNotification,
                    isInterruption: { note in
                        let context = note.userInfo?[AVAudioSession.deactivationContextKey]
                            as? AVAudioSession.DeactivationContext
                        return context?.source == .system
                    }
                )
            }
            #endif
            return .interruptionEnded
        }

        // On older systems a game can resume after the end of an interruption
        // without requiring an explicit play action.
        static let interruptionEnded = Self(
            name: AVAudioSession.interruptionNotification,
            recommendation: { note in
                let raw = note.userInfo?[AVAudioSessionInterruptionTypeKey] as? UInt
                return raw.flatMap(AVAudioSession.InterruptionType.init(rawValue:)) == .ended ? .resume : .ignore
            },
            interruptionName: AVAudioSession.interruptionNotification,
            isInterruption: { note in
                let raw = note.userInfo?[AVAudioSessionInterruptionTypeKey] as? UInt
                return raw.flatMap(AVAudioSession.InterruptionType.init(rawValue:)) == .began
            }
        )
    }

    static let shared = GameAudio { active in
        let session = AVAudioSession.sharedInstance()
        if active {
            try session.setCategory(.playback, mode: .default, options: [.mixWithOthers])
            try session.setActive(true)
        } else {
            try session.setActive(false, options: .notifyOthersOnDeactivation)
        }
    }

    private let queue: DispatchQueue
    private let notifications: NotificationCenter
    private let resumption: ResumptionEvent
    private let setActive: @Sendable (Bool) throws -> Void
    // These fields belong to queue. Work captures the identity of its game session.
    private var observers: [NSObjectProtocol] = []
    private var generation: UUID?
    private var awaitingInterruptionEnd = false
    private var transitionRetry: DispatchWorkItem?

    init(queue: DispatchQueue = DispatchQueue(label: "dev.horizon.game-audio", qos: .userInitiated),
         notifications: NotificationCenter = .default,
         resumption: ResumptionEvent = .system,
         setActive: @escaping @Sendable (Bool) throws -> Void) {
        self.queue = queue
        self.notifications = notifications
        self.resumption = resumption
        self.setActive = setActive
    }

    deinit {
        for observer in observers { notifications.removeObserver(observer) }
    }

    func start() {
        queue.async { [self] in
            guard self.generation == nil else { return }
            let generation = UUID()
            self.generation = generation
            let resumption = self.resumption
            let resumes = self.notifications.addObserver(
                forName: resumption.name, object: nil, queue: nil
            ) { [weak self] note in
                let recommendation = resumption.recommendation(note)
                if case .ignore = recommendation { return }
                self?.queue.async { [weak self] in
                    guard let self, self.generation == generation else { return }
                    switch recommendation {
                    case .resume:
                        self.awaitingInterruptionEnd = false
                        self.applyWithRetry(true, for: generation)
                    case .stayInactive:
                        self.awaitingInterruptionEnd = false
                        self.cancelTransitionRetry()
                    case .ignore: break
                    }
                }
            }
            let interrupts = self.notifications.addObserver(
                forName: resumption.interruptionName, object: nil, queue: nil
            ) { [weak self] note in
                guard resumption.isInterruption(note) else { return }
                self?.queue.async { [weak self] in
                    guard let self, self.generation == generation else { return }
                    self.awaitingInterruptionEnd = true
                    self.cancelTransitionRetry()
                }
            }
            let foreground = self.notifications.addObserver(
                forName: UIApplication.didBecomeActiveNotification, object: nil, queue: nil
            ) { [weak self] _ in
                self?.queue.async { [weak self] in
                    guard let self, self.generation == generation,
                          self.awaitingInterruptionEnd else { return }
                    // The system can omit the interruption-ended notification.
                    self.awaitingInterruptionEnd = false
                    self.applyWithRetry(true, for: generation)
                }
            }
            self.observers = [resumes, interrupts, foreground]
            self.applyWithRetry(true, for: generation)
        }
    }

    func stop() {
        queue.async {
            self.generation = nil
            self.awaitingInterruptionEnd = false
            for observer in self.observers { self.notifications.removeObserver(observer) }
            self.observers = []
            // RemoteIO can open during game loading, before the launcher hands off.
            self.applyWithRetry(false, for: nil)
        }
    }

    private func cancelTransitionRetry() {
        transitionRetry?.cancel()
        transitionRetry = nil
    }

    private func applyWithRetry(_ active: Bool, for generation: UUID?) {
        cancelTransitionRetry()
        guard !applyActive(active) else { return }

        // A system transition can refuse either operation. Retry once unless
        // a newer Play, Quit, or resumption recommendation supersedes it.
        let retry = DispatchWorkItem { [weak self] in
            guard let self, self.generation == generation else { return }
            self.transitionRetry = nil
            _ = self.applyActive(active)
        }
        transitionRetry = retry
        queue.asyncAfter(deadline: .now() + .milliseconds(250), execute: retry)
    }

    private func applyActive(_ active: Bool) -> Bool {
        do {
            try setActive(active)
            return true
        } catch {
            NSLog("Horizon: changing the game audio session to active=\(active) failed: \(error)")
            return false
        }
    }
}
