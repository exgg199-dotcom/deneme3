import SwiftUI
import UIKit

/// The C entry point `horizon-ios-launcher` looks up when a phone with no game files starts downloading them
/// (crates/horizon-client/src/game/interface/import_screen.rs): a SwiftUI screen over the game with the download's
/// progress, speed and time left, read from the game with `download-state`. It goes away by itself once the game
/// files are in and the game loads. Shown even while the launcher is switched off: it only needs this framework.
@_cdecl("horizon_download_present")
public func horizonDownloadPresent(_ submit: HorizonSubmit?) -> Bool {
    Bridge.submit = submit
    Theme.registerFonts()
    Task { @MainActor in presentDownload(attempts: 20) }
    return true
}

@MainActor
private func presentDownload(attempts: Int) {
    let stack = gamePresentationStack()
    guard !stack.contains(where: { $0 is DownloadHostingController }) else { return }
    guard let root = stack.last, let scene = root.viewIfLoaded?.window?.windowScene,
          !root.isBeingPresented, !root.isBeingDismissed else {
        // Not on screen yet: try again shortly, for a couple of seconds at most.
        if attempts > 0 {
            Task { @MainActor in
                try? await Task.sleep(nanoseconds: 100_000_000)
                presentDownload(attempts: attempts - 1)
            }
        }
        return
    }
    let model = DownloadModel()
    let host = DownloadHostingController(rootView: AnyView(DownloadView(model: model)))
    host.orientationMask = root.supportedInterfaceOrientations
    host.currentOrientation = scene.interfaceOrientation
    model.close = { [weak host] in host?.dismiss(animated: true) }
    host.modalPresentationStyle = .fullScreen
    host.modalTransitionStyle = .crossDissolve
    root.present(host, animated: false)
}

/// Keep the presenting scene's orientation and idle policy when the overlay closes.
private final class DownloadHostingController: UIHostingController<AnyView> {
    var orientationMask: UIInterfaceOrientationMask = .allButUpsideDown
    var currentOrientation: UIInterfaceOrientation = .unknown
    private var previousIdleTimerDisabled: Bool?

    override var supportedInterfaceOrientations: UIInterfaceOrientationMask { orientationMask }
    override var preferredInterfaceOrientationForPresentation: UIInterfaceOrientation {
        currentOrientation == .unknown ? super.preferredInterfaceOrientationForPresentation : currentOrientation
    }
    override var preferredStatusBarStyle: UIStatusBarStyle { .lightContent }

    override func viewWillAppear(_ animated: Bool) {
        super.viewWillAppear(animated)
        if previousIdleTimerDisabled == nil {
            previousIdleTimerDisabled = UIApplication.shared.isIdleTimerDisabled
            UIApplication.shared.isIdleTimerDisabled = true
        }
    }

    override func viewWillDisappear(_ animated: Bool) {
        super.viewWillDisappear(animated)
        if let previousIdleTimerDisabled {
            UIApplication.shared.isIdleTimerDisabled = previousIdleTimerDisabled
            self.previousIdleTimerDisabled = nil
        }
    }
}

/// What the game says about the download, asked four times a second, and the speed worked out from it.
@MainActor
final class DownloadModel: ObservableObject {
    enum Phase: Equatable {
        /// No answer yet, or a retry on its way.
        case starting
        case downloading
        /// The archive is here and being unpacked.
        case installing
        case failed(String)
        /// The files are in: the game is loading them.
        case done
    }

    @Published private(set) var phase: Phase = .starting
    @Published private(set) var received: Int64 = 0
    @Published private(set) var total: Int64 = 0
    /// Bytes a second, smoothed over the last few seconds.
    @Published private(set) var speed: Double = 0
    @Published private(set) var version = "26.3"

    /// Dismiss the screen (Present: the download is over, or the player chose another way).
    var close: () -> Void = {}
    private var samples: [(at: TimeInterval, bytes: Int64)] = []
    /// A retry is on its way to the game.
    private var retrying = false
    private var retryGeneration = 0
    private var retryFailure: String?
    private let request: @MainActor (String) async -> [String: Any]
    private let diagnostic: @MainActor (String) -> Void
    private let uptime: @MainActor () -> TimeInterval

    init(request: @escaping @MainActor (String) async -> [String: Any] = { await Bridge.send($0) },
         diagnostic: @escaping @MainActor (String) -> Void = { Diagnostics.record($0) },
         uptime: @escaping @MainActor () -> TimeInterval = { ProcessInfo.processInfo.systemUptime }) {
        self.request = request
        self.diagnostic = diagnostic
        self.uptime = uptime
    }

    var fraction: Double {
        switch phase {
        case .installing, .done: return 1
        default: return total > 0 ? min(1, Double(received) / Double(total)) : 0
        }
    }

    var secondsLeft: Double? {
        guard phase == .downloading, total > received, speed > 1 else { return nil }
        return Double(total - received) / speed
    }

    func run() async {
        while !Task.isCancelled {
            await refresh()
            guard !Task.isCancelled else { return }
            if phase == .done {
                close()
                return
            }
            try? await Task.sleep(nanoseconds: 250_000_000)
        }
    }

    func retry() async {
        guard !retrying, case .failed = phase else { return }
        retryGeneration += 1
        retrying = true
        retryFailure = nil
        phase = .starting
        received = 0
        total = 0
        speed = 0
        samples = []
        let answer = await request("download-retry")
        retrying = false
        if let error = answer.error {
            retryFailure = error
        } else if !answer.acknowledged {
            retryFailure = "Horizon could not confirm that the download restarted."
        }
        if let retryFailure { fail(retryFailure) }
    }

    func refresh() async {
        guard !retrying, !Task.isCancelled else { return }
        let generation = retryGeneration
        let state = await request("download-state")
        guard generation == retryGeneration, !retrying, !Task.isCancelled else { return }
        apply(state)
    }

    /// Let the game's own screen show the other ways in (over Wi-Fi, or a cable to a computer).
    func otherWays() {
        close()
    }

    private func apply(_ state: [String: Any]) {
        switch state["phase"] as? String {
        case "downloading":
            let bytes = max(0, (state["received"] as? NSNumber)?.int64Value ?? 0)
            let reportedTotal = max(0, (state["total"] as? NSNumber)?.int64Value ?? 0)
            // A restarted engine transfer can publish its counters separately.
            // Keep the last coherent sample until both counters agree.
            guard reportedTotal == 0 || bytes <= reportedTotal else { return }
            retryFailure = nil
            if total > 0, reportedTotal != total {
                samples = []
                speed = 0
            }
            total = reportedTotal
            if let version = state["version"] as? String {
                self.version = version
            }
            measure(bytes)
            received = bytes
            phase = .downloading
        case "installing":
            retryFailure = nil
            phase = .installing
            speed = 0
        case "failed":
            // The previous transfer can still be marked failed after a rejected retry.
            fail(retryFailure ?? state.error ?? "The download stopped.")
        case "done":
            retryFailure = nil
            phase = .done
        default:
            fail(retryFailure ?? state.error ?? "Horizon could not read the download status.")
        }
    }

    private func fail(_ error: String) {
        let failure = Phase.failed(error)
        guard phase != failure else { return }
        diagnostic("Game files (game download) failed: \(Diagnostics.redact(error))")
        phase = failure
        speed = 0
        samples = []
    }

    /// The speed over the last four seconds, eased so the number doesn't jump with every read.
    private func measure(_ bytes: Int64) {
        let now = uptime()
        let sampleWindow = 4.0
        if let last = samples.last, bytes < last.bytes || now - last.at > sampleWindow {
            // Restarted the transfer, or resumed polling after a pause.
            samples = []
            speed = 0
        }
        samples.append((now, bytes))
        samples.removeAll { now - $0.at > sampleWindow }
        guard let first = samples.first, let last = samples.last else { return }
        let span = last.at - first.at
        guard span > 0.5 else { return }
        if last.bytes == first.bytes {
            speed = 0
            return
        }
        let rate = Double(last.bytes - first.bytes) / span
        speed = speed == 0 ? rate : speed * 0.7 + rate * 0.3
    }
}

/// The download status, progress and available recovery actions.
struct DownloadView: View {
    @ObservedObject var model: DownloadModel

    var body: some View {
        ZStack {
            Theme.background.ignoresSafeArea()
            ScrollView {
                VStack(alignment: .leading, spacing: 22) {
                    VStack(alignment: .leading, spacing: 8) {
                        Text("HORIZON")
                            .font(Theme.pixel(14, .bold))
                            .foregroundStyle(Theme.accentText)
                            .tracking(2)
                        Text(title)
                            .font(Theme.pixel(28, .bold))
                            .foregroundStyle(Theme.text)
                        Text(subtitle)
                            .font(Theme.sans(15))
                            .foregroundStyle(Theme.textMuted)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                    HStack(spacing: 14) {
                        ProgressView(value: model.fraction)
                            .progressViewStyle(.linear)
                            .tint(failed ? Theme.danger : Theme.accent)
                            .scaleEffect(x: 1, y: 2, anchor: .center)
                            .animation(.easeOut(duration: 0.25), value: model.fraction)
                        if !failed {
                            ProgressView()
                                .progressViewStyle(.circular)
                                .tint(Theme.accentText)
                        } else {
                            Image(systemName: "exclamationmark.triangle.fill")
                                .foregroundStyle(Theme.danger)
                        }
                    }
                    .frame(minHeight: 28)
                    if case .failed = model.phase {
                        VStack(spacing: 10) {
                            PrimaryButton(title: "Try again", icon: "arrow.clockwise") {
                                Task { await model.retry() }
                            }
                            SecondaryButton(title: "Other ways to get them", icon: "wifi") {
                                model.otherWays()
                            }
                        }
                    }
                    stats
                    Text(footnote)
                        .font(Theme.sans(12))
                        .foregroundStyle(Theme.textFaint)
                        .fixedSize(horizontal: false, vertical: true)
                }
                .padding(.horizontal, 28)
                .padding(.vertical, 24)
                .frame(maxWidth: 620)
                .frame(maxWidth: .infinity)
            }
        }
        .task { await model.run() }
    }

    private var failed: Bool {
        if case .failed = model.phase { return true }
        return false
    }

    private var title: String {
        switch model.phase {
        case .starting: return "Getting the game files"
        case .downloading: return "Downloading game files"
        case .installing: return "Installing game files"
        case .failed: return "The download stopped"
        case .done: return "Starting Horizon"
        }
    }

    private var subtitle: String {
        switch model.phase {
        case .starting:
            return "Connecting to the download server…"
        case .downloading:
            return model.total > 0
                ? "Minecraft \(model.version) · \(bytes(model.total)) in all"
                : "Minecraft \(model.version)"
        case .installing:
            return "Unpacking the archive into the game's folder. This takes a minute."
        case .failed(let message):
            return message
        case .done:
            return "The game files are in."
        }
    }

    private var footnote: String {
        if case .failed = model.phase {
            return "Trying again picks up from what already arrived. The other ways send the files from a computer, over Wi-Fi or a cable."
        }
        return "Keep Horizon open while this runs: the screen stays on, and a dropped connection picks up where it stopped."
    }

    private var stats: some View {
        VStack(spacing: 0) {
            row("Progress", model.phase == .starting ? "–" : "\(Int((model.fraction * 100).rounded(.down)))%")
            Divider().overlay(Theme.border)
            row("Downloaded", model.total > 0
                ? "\(bytes(model.received)) of \(bytes(model.total))"
                : (model.received > 0 ? bytes(model.received) : "–"))
            Divider().overlay(Theme.border)
            row("Speed", model.phase == .downloading && model.speed > 0 ? "\(bytes(Int64(model.speed)))/s" : "–")
            Divider().overlay(Theme.border)
            row("Time left", timeLeft)
        }
        .background(Theme.surface, in: RoundedRectangle(cornerRadius: 10))
        .overlay(RoundedRectangle(cornerRadius: 10).strokeBorder(Theme.border))
    }

    private func row(_ label: String, _ value: String) -> some View {
        HStack {
            Text(label)
                .font(Theme.sans(14))
                .foregroundStyle(Theme.textMuted)
            Spacer()
            Text(value)
                .font(Theme.mono(14))
                .foregroundStyle(Theme.text)
                .monospacedDigit()
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 12)
    }

    private var timeLeft: String {
        switch model.phase {
        case .installing: return "almost done"
        case .downloading:
            guard let seconds = model.secondsLeft else { return "working it out…" }
            let format = DateComponentsFormatter()
            format.allowedUnits = seconds >= 3600 ? [.hour, .minute] : [.minute, .second]
            format.unitsStyle = .abbreviated
            format.maximumUnitCount = 2
            return format.string(from: seconds) ?? "–"
        default: return "–"
        }
    }

    private func bytes(_ count: Int64) -> String {
        ByteCountFormatter.string(fromByteCount: count, countStyle: .file)
    }
}
