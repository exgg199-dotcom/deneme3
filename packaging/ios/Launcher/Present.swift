import SwiftUI
import UIKit

/// The C entry point `horizon-ios-launcher` looks up: show the launcher while the engine is stopped or starting. Play dismisses it; the game's Quit Game asks for it again. `submit` is how the
/// launcher asks the game things (Bridge.swift). Called on the main thread; the presenting waits a turn of the
/// run loop, because at startup the game's view controller may not be on screen yet while its first frame is
/// being set up.
@_cdecl("horizon_launcher_present")
public func horizonLauncherPresent(_ submit: HorizonSubmit?) -> Bool {
    Diagnostics.record("Launcher presented")
    Bridge.submit = submit
    Theme.registerFonts()
    DispatchQueue.main.async { present() }
    return true
}

// Presentation requests can overlap while the Rust bridge acknowledges them.
private var presentationPending = false

@MainActor
private func present() {
    // Reserve the whole attempt, including time spent waiting for a window.
    guard !presentationPending,
          !gamePresentationStack().contains(where: { $0 is LauncherHostingController }) else { return }
    presentationPending = true
    Task { @MainActor in
        let root = await prepareLauncherPresentation(
            attempts: 21,
            presenter: {
                guard let root = gameViewController(), root.viewIfLoaded?.window != nil,
                      !root.isBeingPresented, !root.isBeingDismissed else { return nil }
                return root
            },
            setCovered: { visible in
                let answer = await Bridge.send("launcher-visible \(visible)")
                return answer.acknowledged && answer.error == nil
            },
            pause: { try? await Task.sleep(nanoseconds: 100_000_000) },
            startGame: {
                let answer = await Bridge.send("launcher-start")
                if answer.acknowledged, answer.error == nil {
                    GameAudio.shared.start()
                }
            }
        )
        guard let root else {
            presentationPending = false
            return
        }
        TouchGestureBridge.shared.attach(to: root.view)
        let host = LauncherHostingController(rootView: AnyView(EmptyView()))
        host.rootView = AnyView(
            HomeView(onPlay: { [weak host] in
                guard let host else { return }
                GameAudio.shared.start()
                let scene = host.view.window?.windowScene
                host.dismiss(animated: true) {
                    Task { @MainActor in
                        _ = await Bridge.send("launcher-visible false")
                        turn(scene, to: .landscape)
                    }
                }
            })
        )
        host.modalPresentationStyle = .fullScreen
        host.modalPresentationCapturesStatusBarAppearance = true
        host.modalTransitionStyle = .crossDissolve
        TouchGestureBridge.shared.suspend()
        root.present(host, animated: false) {
            presentationPending = false
            turn(host.view.window?.windowScene, to: UIDevice.current.userInterfaceIdiom == .pad ? .all : .portrait)
        }
    }
}

/// An acknowledgement can arrive after UIKit has replaced the presenting
/// window. Retry with its replacement, or let the game start without a launcher.
@MainActor
func prepareLauncherPresentation(
    attempts: Int,
    presenter: () -> UIViewController?,
    setCovered: (Bool) async -> Bool,
    pause: () async -> Void,
    startGame: () async -> Void
) async -> UIViewController? {
    var requestedCover = false
    for attempt in 0..<attempts {
        if let root = presenter() {
            // Set the underlying controller's orientation mask before UIKit presents.
            requestedCover = true
            guard await setCovered(true) else { break }
            if presenter() === root { return root }
        }
        if attempt + 1 < attempts { await pause() }
    }
    await startGame()
    // A failed reply does not rule out a late visibility change in the engine.
    // Queue the reversal after the fallback start so the game can be revealed.
    if requestedCover { _ = await setCovered(false) }
    return nil
}

/// Ask the scene to turn to `orientations` now, rather than when the phone is next moved.
func turn(_ scene: UIWindowScene?, to orientations: UIInterfaceOrientationMask) {
    guard let scene else { return }
    for window in scene.windows {
        window.rootViewController?.setNeedsUpdateOfSupportedInterfaceOrientations()
        var top = window.rootViewController
        while let next = top?.presentedViewController {
            top = next
        }
        top?.setNeedsUpdateOfSupportedInterfaceOrientations()
    }
    scene.requestGeometryUpdate(.iOS(interfaceOrientations: orientations)) { error in
        NSLog("Horizon orientation request failed: %@", error.localizedDescription)
    }
}

/// The topmost view controller in the game window.
func gameViewController() -> UIViewController? {
    gamePresentationStack().last
}

/// Includes the launcher or download overlay even while it presents a sheet.
func gamePresentationStack() -> [UIViewController] {
    let windows = UIApplication.shared.connectedScenes
        .filter { $0.session.role == .windowApplication }
        .compactMap { $0 as? UIWindowScene }
        .flatMap(\.windows)
    let window = windows.first(where: \.isKeyWindow) ?? windows.first
    return presentationStack(from: window?.rootViewController)
}

func presentationStack(from root: UIViewController?) -> [UIViewController] {
    var controllers: [UIViewController] = []
    var next = root
    while let controller = next {
        controllers.append(controller)
        next = controller.presentedViewController
    }
    return controllers
}

/// The launcher stands upright, as the design does; the game under it is held sideways by its own view
/// controller after dismissal. While covered, that controller permits both orientations.
private final class LauncherHostingController: UIHostingController<AnyView> {
    override var supportedInterfaceOrientations: UIInterfaceOrientationMask {
        UIDevice.current.userInterfaceIdiom == .pad ? .all : .portrait
    }
    override var preferredInterfaceOrientationForPresentation: UIInterfaceOrientation {
        guard UIDevice.current.userInterfaceIdiom == .pad else { return .portrait }
        return UIApplication.shared.connectedScenes
            .filter { $0.session.role == .windowApplication }
        .compactMap { $0 as? UIWindowScene }
            .first(where: { $0.activationState == .foregroundActive })?.interfaceOrientation ?? .portrait
    }
    override var preferredStatusBarStyle: UIStatusBarStyle { .default }
    override var prefersStatusBarHidden: Bool { true }
}
