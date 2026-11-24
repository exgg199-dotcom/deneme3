import XCTest
import UIKit
import UIKit.UIGestureRecognizerSubclass
@testable import LauncherPreview

final class TouchControlsTests: XCTestCase {
    private var suite: String!
    private var defaults: UserDefaults!
    override func setUp() {
        suite = "horizon.touch.fixture." + UUID().uuidString
        defaults = UserDefaults(suiteName: suite)!
    }
    override func tearDown() { defaults.removePersistentDomain(forName: suite) }

    @MainActor func testNativeControlsFollowRenderViewScaleWhenItDiffersFromTheScreen() throws {
        var configuration = TouchConfiguration()
        let canvas = CGSize(width: 600, height: 300)
        let click = CGPoint(x: 300, y: 150)
        configuration.move(.leftClick, to: click, in: canvas)
        var custom = CustomTouchButton()
        custom.centre = TouchCentre(x: 0.8, y: 0.25)
        configuration.customButtons = [custom]
        let view = UIView(frame: CGRect(origin: .zero, size: canvas))
        view.contentScaleFactor = UIScreen.main.scale + 1
        let bridge = TouchGestureBridge(configuration: { configuration })

        bridge.attach(to: view)
        _ = bridge.poll(screen: 1, flags: 1, width: 600, height: 300,
                        scale: Double(view.contentScaleFactor), capacity: 64)

        let control = try XCTUnwrap(view.subviews.compactMap { $0 as? CustomTouchView }.first)
        XCTAssertEqual(control.frame, custom.rect(in: canvas))
        XCTAssertFalse(bridge.gestureRecognizer(TouchSurfaceTap(), shouldReceive: PositionedTouch(click)))
    }

    @MainActor func testMenuScrollCannotCancelGameplayFingers() throws {
        let bridge = TouchGestureBridge(configuration: { TouchConfiguration() })
        let view = UIView(frame: CGRect(x: 0, y: 0, width: 600, height: 300))
        bridge.attach(to: view)
        let scroll = try XCTUnwrap(view.gestureRecognizers?.compactMap { $0 as? UIPanGestureRecognizer }.first)
        XCTAssertTrue(scroll.cancelsTouchesInView)
        let touch = PositionedTouch(CGPoint(x: 300, y: 110))
        // Title and in-world menus retain two-finger scrolling.
        for flags: UInt32 in [8, 0] {
            _ = bridge.poll(screen: 91, flags: flags, width: 600, height: 300, scale: 1, capacity: 64)
            XCTAssertTrue(bridge.gestureRecognizer(scroll, shouldReceive: touch))
        }
        _ = bridge.poll(screen: 91, flags: 1, width: 600, height: 300, scale: 1, capacity: 64)
        XCTAssertFalse(bridge.gestureRecognizer(scroll, shouldReceive: touch),
                       "A menu-only recognizer must not cancel world camera fingers")
    }

    @MainActor func testScreenChangeCancelsTheActiveUIKitRecognizer() throws {
        let bridge = TouchGestureBridge(configuration: { TouchConfiguration() })
        let view = UIView(frame: CGRect(x: 0, y: 0, width: 600, height: 300))
        bridge.attach(to: view)
        let recognizers = try XCTUnwrap(view.gestureRecognizers)
        let tracker = try XCTUnwrap(recognizers.compactMap { $0 as? TouchHotbarTracker }.first)
        func poll(screen: UInt64, flags: UInt32 = 0) -> [[Double]] {
            bridge.poll(screen: screen, flags: flags, width: 600, height: 300, scale: 1, capacity: 64)
        }
        XCTAssertTrue(recognizers.allSatisfy { !$0.isEnabled })
        _ = poll(screen: 1)
        tracker.state = .began
        XCTAssertEqual(tracker.state, .began)
        XCTAssertTrue(poll(screen: 2).isEmpty)
        XCTAssertNotEqual(tracker.state, .began)
        XCTAssertTrue(recognizers.allSatisfy { $0.isEnabled })

        bridge.suspend()
        XCTAssertTrue(recognizers.allSatisfy { !$0.isEnabled })
        XCTAssertTrue(poll(screen: 2, flags: 2).isEmpty)
        XCTAssertTrue(recognizers.allSatisfy { !$0.isEnabled })
        XCTAssertEqual(poll(screen: 2).map { $0[0] }, [0])
        XCTAssertTrue(recognizers.allSatisfy { $0.isEnabled })
    }

    @MainActor func testLongPressReleasesItsOriginalActionAndCannotEndOnANewScreen() {
        let bridge = TouchGestureBridge(configuration: { TouchConfiguration() })
        let view = UIView(frame: CGRect(x: 0, y: 0, width: 600, height: 300))
        bridge.attach(to: view)
        func poll(_ screen: UInt64) -> [[Double]] {
            bridge.poll(screen: screen, flags: 1, width: 600, height: 300,
                        scale: Double(view.contentScaleFactor), capacity: 64)
        }
        _ = poll(1)
        let hold = FixedGestureHold()
        hold.point = CGPoint(x: 300, y: 299)
        _ = bridge.perform(NSSelectorFromString("held:"), with: hold)
        XCTAssertEqual(poll(1).map { [$0[0], $0[1], $0[4]] }, [[2, 1, 4]])
        hold.phase = .ended
        hold.point = CGPoint(x: 300, y: 100)
        _ = bridge.perform(NSSelectorFromString("held:"), with: hold)
        XCTAssertEqual(poll(1).map { [$0[0], $0[1], $0[4]] }, [[2, 3, 4]])

        hold.phase = .began
        _ = bridge.perform(NSSelectorFromString("held:"), with: hold)
        XCTAssertTrue(poll(2).isEmpty)
        hold.phase = .cancelled
        _ = bridge.perform(NSSelectorFromString("held:"), with: hold)
        XCTAssertTrue(poll(2).isEmpty)
    }

    @MainActor func testLongPressOnlySuppressesTapsFromItsOwnFinger() throws {
        let bridge = TouchGestureBridge()
        let view = UIView(frame: CGRect(x: 0, y: 0, width: 600, height: 300))
        bridge.attach(to: view)
        _ = bridge.poll(screen: 12, flags: 1, width: 600, height: 300,
                        scale: Double(view.contentScaleFactor), capacity: 64)
        let hold = try XCTUnwrap(view.gestureRecognizers?.compactMap { $0 as? TouchSurfaceHold }.first)
        let taps = try XCTUnwrap(view.gestureRecognizers?.compactMap { $0 as? TouchSurfaceTap })
        let single = try XCTUnwrap(taps.first { $0.numberOfTapsRequired == 1 })
        let double = try XCTUnwrap(taps.first { $0.numberOfTapsRequired == 2 })
        let held = PositionedTouch(CGPoint(x: 320, y: 200))
        let other = PositionedTouch(CGPoint(x: 300, y: 100))
        single.trackTapStart(other)
        double.trackTapStart(held)
        hold.trackHoldStart(held)
        hold.state = .began
        _ = bridge.perform(NSSelectorFromString("held:"), with: hold)

        XCTAssertTrue(single.clickEligible)
        XCTAssertFalse(double.clickEligible)
        _ = bridge.gestureRecognizer(single, shouldReceive: other)
        XCTAssertFalse(double.clickEligible)
    }

    @MainActor func testHotbarTapsKeepTheirOwnOriginsAndDoubleTapStaysInOneSlot() {
        let bridge = TouchGestureBridge(configuration: { TouchConfiguration() })
        let view = UIView(frame: CGRect(x: 0, y: 0, width: 600, height: 300))
        bridge.attach(to: view)
        func poll() -> [[Double]] {
            bridge.poll(screen: 1, flags: 1, width: 600, height: 300,
                        scale: Double(view.contentScaleFactor), capacity: 64)
        }
        _ = poll()
        let original = PositionedTouch(CGPoint(x: 300, y: 299))
        let otherFinger = PositionedTouch(CGPoint(x: 320, y: 299))
        let sameSlot = PositionedTouch(CGPoint(x: 301, y: 299))
        func slot(_ point: CGPoint) -> Int {
            TouchHotbar.slot(point: point, pixels: view.bounds.size, scale: 1)
        }
        let tap = TouchSurfaceTap()
        tap.trackTapStart(original)
        _ = bridge.gestureRecognizer(UITapGestureRecognizer(), shouldReceive: otherFinger)
        tap.trackTapStart(otherFinger)
        XCTAssertEqual(tap.hotbarSlot(using: slot), 4)

        for second in [otherFinger, sameSlot] {
            let double = TouchSurfaceTap()
            double.numberOfTapsRequired = 2
            double.trackTapStart(original)
            double.trackTapEnd([original])
            double.trackTapStart(second)
            XCTAssertEqual(double.hotbarSlot(using: slot), second === sameSlot ? 4 : nil)
        }
    }

    @MainActor func testHotbarTrackerOnlyStartsForAnEnabledSlideInsideTheVisibleHotbar() {
        var configuration = TouchConfiguration()
        let view = UIView(frame: CGRect(x: 0, y: 0, width: 600, height: 300))
        let bridge = TouchGestureBridge(configuration: { configuration })
        bridge.attach(to: view)
        _ = bridge.poll(screen: 1, flags: 1, width: 600, height: 300,
                        scale: Double(view.contentScaleFactor), capacity: 64)
        let tracker = TouchHotbarTracker()
        let inside = PositionedTouch(CGPoint(x: 300, y: 299))
        XCTAssertFalse(bridge.gestureRecognizer(tracker, shouldReceive: inside))
        configuration.slideableHotbar = true
        XCTAssertTrue(bridge.gestureRecognizer(tracker, shouldReceive: inside))
        let outside = PositionedTouch(CGPoint(x: 300, y: 300))
        XCTAssertFalse(bridge.gestureRecognizer(tracker, shouldReceive: outside))
    }

    @MainActor func testTitleTapsIgnoreHiddenGameplayHitboxesAndGesturePreference() {
        var config = TouchConfiguration()
        config.mouseGestures = false
        let size = CGSize(width: 600, height: 300)
        config.move(.leftClick, to: CGPoint(x: 300, y: 150), in: size)
        let bridge = TouchGestureBridge(configuration: { config })
        let surface = UIView(frame: CGRect(origin: .zero, size: size))
        bridge.attach(to: surface)
        let scale = Double(surface.contentScaleFactor)
        _ = bridge.poll(screen: 70, flags: 8, width: 600, height: 300, scale: scale, capacity: 64)
        let tap = TouchSurfaceTap()
        for button in [TouchButton.leftClick, .rightClick, .joystick] {
            let rect = config.rect(button, in: size)
            let touch = PositionedTouch(CGPoint(x: rect.midX, y: rect.midY))
            XCTAssertTrue(bridge.gestureRecognizer(tap, shouldReceive: touch))
        }
        // Entering a world restores both the control hitboxes and gesture setting.
        _ = bridge.poll(screen: 71, flags: 0, width: 600, height: 300, scale: scale, capacity: 64)
        XCTAssertFalse(bridge.gestureRecognizer(tap, shouldReceive: PositionedTouch(CGPoint(x: 300, y: 150))))
        XCTAssertFalse(bridge.gestureRecognizer(tap, shouldReceive: PositionedTouch(CGPoint(x: 300, y: 80))))
        _ = bridge.poll(screen: 72, flags: 8, width: 600, height: 300, scale: scale, capacity: 64)
        XCTAssertTrue(bridge.gestureRecognizer(tap, shouldReceive: PositionedTouch(CGPoint(x: 300, y: 150))))
    }
    @MainActor func testMoreAndHideOnlyExcludeVisibleControlsFromGestures() {
        var config = TouchConfiguration()
        let size = CGSize(width:600,height:300)
        config.move(.debug,to:CGPoint(x:300,y:150),in:size)
        let bridge = TouchGestureBridge(configuration:{config})
        let view = UIView(frame:CGRect(origin:.zero,size:size)); bridge.attach(to:view)
        let tap = TouchSurfaceTap(), point = PositionedTouch(CGPoint(x:300,y:150))
        let scale = Double(view.contentScaleFactor)
        _ = bridge.poll(screen:1,flags:0,width:600,height:300,scale:scale,capacity:64)
        XCTAssertTrue(bridge.gestureRecognizer(tap,shouldReceive:point))
        _ = bridge.poll(screen:1,flags:32,width:600,height:300,scale:scale,capacity:64)
        XCTAssertFalse(bridge.gestureRecognizer(tap,shouldReceive:point))
        _ = bridge.poll(screen:1,flags:16|32,width:600,height:300,scale:scale,capacity:64)
        XCTAssertTrue(bridge.gestureRecognizer(tap,shouldReceive:point))
        let corner = PositionedTouch(CGPoint(x:590,y:25))
        XCTAssertTrue(bridge.gestureRecognizer(tap,shouldReceive:corner))
        _ = bridge.poll(screen:1,flags:64,width:600,height:300,scale:scale,capacity:64)
        XCTAssertFalse(bridge.gestureRecognizer(tap,shouldReceive:corner))
    }
    func testNewInstallAndResetUseDefaultButtonsVisibleInCaptures() {
        var config = TouchPreferences(defaults: defaults).configuration
        XCTAssertTrue(config.showControls)
        XCTAssertEqual(config.snapshot().1 & 1, 0)
        let size = CGSize(width: 650, height: 300)
        for button in TouchButton.allCases { XCTAssertEqual(config.rect(button, in: size), button.rect(in: size)) }
        config.move(.jump, to: CGPoint(x: 200, y: 100), in: size)
        config.centres = [:]
        XCTAssertEqual(config.rect(.jump, in: size), TouchButton.jump.rect(in: size))
    }
    func testSaveReloadClampsAndResizesTheActualButtonCentres() {
        let model = TouchPreferences(defaults: defaults)
        var draft = model.configuration
        draft.move(.jump, to: CGPoint(x: 325, y: 120), in: CGSize(width: 650, height: 300))
        XCTAssertTrue(model.configuration.centres.isEmpty, "Draft must not affect the running game")
        model.save(draft)
        let loaded = TouchPreferences(defaults: defaults).configuration
        XCTAssertEqual(loaded, draft)
        for size in [CGSize(width: 650, height: 300), CGSize(width: 1194, height: 834)] {
            let r = loaded.rect(.jump, in: size)
            XCTAssertEqual(r.midX / size.width, 0.5, accuracy: 0.00001)
            XCTAssertEqual(r.midY / size.height, 0.4, accuracy: 0.00001)
        }
        draft.move(.forward, to: CGPoint(x: -9999, y: 9999), in: CGSize(width: 650, height: 300))
        let r = draft.rect(.forward, in: CGSize(width: 650, height: 300))
        XCTAssertEqual(r.minX, 0); XCTAssertEqual(r.maxY, 300)
    }

    func testResetKeepsLargeCustomLayoutsFromOverlapping() {
        var layout = TouchConfiguration()
        for index in 0..<40 {
            var button = CustomTouchButton()
            button.name = "Button \(index)"
            button.bindings = [65 + index % 26, 0, 0, 0]
            layout.customButtons.append(button)
        }
        let controls = layout.customButtons
        layout.resetLayout()

        XCTAssertEqual(layout.customButtons.map(\.id), controls.map(\.id))
        XCTAssertEqual(layout.customButtons.map(\.keys), controls.map(\.keys))
        XCTAssertTrue(layout.customButtons.allSatisfy { $0.size.valid })
        for canvas in [CGSize(width: 650, height: 300), CGSize(width: 1_194, height: 834),
                       CGSize(width: 500, height: 500)] {
            let buttons = layout.customButtons.map { $0.rect(in: canvas) }
            for (index, frame) in buttons.enumerated() {
                XCTAssertFalse(buttons.dropFirst(index + 1).contains { $0.intersects(frame) })
            }
        }
    }

    func testResetIncludesCustomGeometryButPreservesBindingsUntilSaved() {
        let preferences = TouchPreferences(defaults: defaults)
        var original = TouchConfiguration()
        let canvas = CGSize(width: 650, height: 300)
        original.move(.jump, to: CGPoint(x: 200, y: 70), in: canvas)
        original.resize(.jump, factor: 1.5, in: canvas)
        original.opacities[String(TouchButton.jump.rawValue)] = 0.5
        original.showControls = false
        original.forwardLock = true
        var custom = CustomTouchButton()
        custom.name = "Inventory chord"
        custom.bindings = [340, 69, 0, 0]
        custom.centre = TouchCentre(x: 0.9, y: 0.1)
        custom.size = TouchSize(width: 0.4, height: 0.3)
        custom.opacity = 0.2
        custom.inMenu = false
        custom.toggle = true
        var second = custom
        second.id = UUID()
        original.customButtons = [custom, second]
        preferences.save(original)

        var draft = preferences.configuration
        draft.resetLayout()
        XCTAssertEqual(preferences.configuration, original)
        XCTAssertTrue(draft.centres.isEmpty)
        XCTAssertTrue(draft.sizes.isEmpty)
        XCTAssertTrue(draft.opacities.isEmpty)
        XCTAssertFalse(draft.showControls)
        XCTAssertTrue(draft.forwardLock)
        XCTAssertNotEqual(draft.customButtons[0].centre, draft.customButtons[1].centre)
        let appearance = CustomTouchButton()
        for (reset, before) in zip(draft.customButtons, original.customButtons) {
            XCTAssertEqual(reset.size, appearance.size)
            XCTAssertEqual(reset.opacity, appearance.opacity)
            var unchanged = reset
            unchanged.centre = before.centre
            unchanged.size = before.size
            unchanged.opacity = before.opacity
            XCTAssertEqual(unchanged, before)
        }
        preferences.save(draft)
        XCTAssertEqual(TouchPreferences(defaults: defaults).configuration, draft)
    }
    func testHidingPaintNeverRemovesCoordinatesAndShowingRestoresVisibility() {
        let model = TouchPreferences(defaults: defaults)
        var draft = model.configuration
        draft.move(.jump, to: CGPoint(x: 300, y: 100), in: CGSize(width: 650, height: 300))
        model.save(draft)
        let visible = model.snapshot()
        XCTAssertEqual(visible.1 & 1, 0)
        draft.showControls = false; model.save(draft)
        let hidden = model.snapshot()
        XCTAssertEqual(hidden.1 & 1, 1)
        XCTAssertEqual(hidden.0[10], visible.0[10]); XCTAssertEqual(hidden.0[11], visible.0[11])
        draft.showControls = true; model.save(draft)
        XCTAssertEqual(model.snapshot().1 & 1, 0)
    }
    func testLegacyLayoutsKeepPositionsAndGainCursorMode() throws {
        let old = Data(#"{"centres":{"5":{"x":0.7,"y":0.3}},"showControls":true}"#.utf8)
        let config = try JSONDecoder().decode(TouchConfiguration.self, from: old)
        XCTAssertEqual(config.centres["5"], TouchCentre(x:0.7,y:0.3))
        XCTAssertTrue(config.virtualMouse); XCTAssertFalse(config.joystick)
        XCTAssertEqual(config.snapshot().0.count,100)
        XCTAssertEqual(config.snapshot().1 & 2,2)
    }
    func testUnreadableJoystickPreferenceUsesTheCurrentDefault() throws {
        let invalid = Data(#"{"joystick":"invalid"}"#.utf8)
        let settings = try JSONDecoder().decode(TouchConfiguration.self, from: invalid)
        XCTAssertEqual(settings.joystick, TouchConfiguration().joystick)
        XCTAssertTrue(settings.buttons.contains(.joystick))
    }

    func testMalformedDataAndCoordinatesCannotMoveButtonsOffscreen() {
        defaults.set(Data("invalid".utf8), forKey: TouchPreferences.key)
        XCTAssertEqual(TouchPreferences(defaults: defaults).configuration, TouchConfiguration())
        var config = TouchConfiguration()
        for c in [TouchCentre(x: .nan, y: 0), TouchCentre(x: .infinity, y: 1), TouchCentre(x: 100, y: -5)] {
            config.centres["5"] = c
            XCTAssertEqual(config.rect(.jump, in: CGSize(width: 650, height: 300)), TouchButton.jump.rect(in: CGSize(width: 650, height: 300)))
            XCTAssertTrue(config.snapshot().0[10].isNaN)
        }
    }
    func testEditorUsesPhysicalPixelsAndGameGUIScaleWithoutAnInsetCanvas() {
        let phone = TouchEditorGeometry(size: CGSize(width: 844, height: 390), displayScale: 3, limit: 0)
        XCTAssertEqual(phone.gui, CGSize(width: 633, height: 293))
        XCTAssertEqual(phone.pointsPerUnit, 4.0 / 3.0, accuracy: 0.0001)
        let pad = TouchEditorGeometry(size: CGSize(width: 1210, height: 834), displayScale: 2, limit: 0)
        XCTAssertEqual(pad.gui, CGSize(width: 404, height: 278))
        XCTAssertEqual(pad.pointsPerUnit, 3)
        let manual = TouchEditorGeometry(size: CGSize(width: 844, height: 390), displayScale: 3, limit: 2)
        XCTAssertEqual(manual.gui, CGSize(width: 1266, height: 585))
        XCTAssertEqual(manual.pointsPerUnit, 2.0 / 3.0, accuracy: 0.0001)
    }
    func testFactoryMovementControlsFitCompactSurfaces() {
        for size in [CGSize(width: 75, height: 64), CGSize(width: 30, height: 40)] {
            for button: TouchButton in [.forward, .back, .left, .right, .joystick] {
                let rect = button.rect(in: size)
                XCTAssertGreaterThan(rect.width, 0)
                XCTAssertGreaterThan(rect.height, 0)
                XCTAssertGreaterThanOrEqual(rect.minX, -0.0001)
                XCTAssertGreaterThanOrEqual(rect.minY, -0.0001)
                XCTAssertLessThanOrEqual(rect.maxX, size.width + 0.0001)
                XCTAssertLessThanOrEqual(rect.maxY, size.height + 0.0001)
            }
        }
    }

    func testToolbarFitsNarrowAndShortCanvases() {
        let narrow = TouchEditorToolbarGeometry(canvas: CGSize(width: 240, height: 120))
        XCTAssertTrue(narrow.compact)
        XCTAssertEqual(narrow.width, 216)
        let left = narrow.fittedCentre(CGPoint(x: -20, y: -20))
        let right = narrow.fittedCentre(CGPoint(x: 1_000, y: 1_000))
        XCTAssertEqual(left, CGPoint(x: 108, y: 60))
        XCTAssertEqual(right, CGPoint(x: 132, y: 60))
        XCTAssertLessThanOrEqual(right.x + narrow.width / 2, 240)

        let tablet = TouchEditorToolbarGeometry(canvas: CGSize(width: 950, height: 650))
        XCTAssertFalse(tablet.compact)
        XCTAssertEqual(tablet.width, 500)
        XCTAssertEqual(tablet.fittedCentre(CGPoint(x: 800, y: 900)), CGPoint(x: 700, y: 570))
    }

    func testEditorUsesTheSavedUnicodeGuiScale() throws {
        let options = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: options) }
        try Data("forceUnicodeFont:false\r\nguiScale:0\r\nforceUnicodeFont:true\r\nguiScale:3\r\n".utf8)
            .write(to: options)
        let saved = TouchEditorGeometry.savedOptions(at: options)
        XCTAssertEqual(saved.limit, 3)
        XCTAssertTrue(saved.forceUnicode)

        let size = CGSize(width: 900, height: 600)
        let normal = TouchEditorGeometry(size: size, displayScale: 2, limit: saved.limit)
        XCTAssertEqual(normal.gui, CGSize(width: 600, height: 400))
        let unicode = TouchEditorGeometry(size: size, displayScale: 2,
                                          limit: saved.limit, forceUnicode: saved.forceUnicode)
        XCTAssertEqual(unicode.gui, CGSize(width: 450, height: 300))
        XCTAssertEqual(unicode.pointsPerUnit, 2)
        let sceneScale = TouchEditorGeometry(size: size, displayScale: 3,
                                             limit: saved.limit, forceUnicode: saved.forceUnicode)
        XCTAssertEqual(sceneScale.gui, CGSize(width: 675, height: 450))
        XCTAssertEqual(sceneScale.pointsPerUnit, 4.0 / 3.0, accuracy: 0.0001)
    }

    func testResizingKeepsTheSavedAnchorAcrossCanvasChanges() {
        let portrait = CGSize(width: 300, height: 600)
        let landscape = CGSize(width: 800, height: 400)
        let button = TouchButton.jump
        let key = String(button.rawValue)
        let anchor = TouchCentre(x: 0.97, y: 0.95)
        var config = TouchConfiguration()
        config.centres[key] = anchor
        config.sizes[key] = TouchSize(width: 0.4, height: 0.4)
        XCTAssertLessThan(config.rect(button, in: portrait).midX, portrait.width * anchor.x)

        config.resize(button, factor: 0.2, in: portrait)
        XCTAssertEqual(config.centres[key], anchor)
        let rect = config.rect(button, in: landscape)
        XCTAssertEqual(rect.midX, landscape.width * anchor.x, accuracy: 0.001)
        XCTAssertEqual(rect.midY, landscape.height * anchor.y, accuracy: 0.001)
    }

    func testRectangularButtonKeepsProportionsAtBothResizeLimits() {
        let canvas = CGSize(width: 800, height: 400)
        let button = TouchButton.chat
        var config = TouchConfiguration()
        let initial = config.rect(button, in: canvas)
        for factor in [100.0, 0.001, 4.0] {
            config.resize(button, factor: factor, in: canvas)
            let rect = config.rect(button, in: canvas)
            XCTAssertEqual(rect.width / rect.height, initial.width / initial.height, accuracy: 0.00001)
            XCTAssertTrue(config.sizes[String(button.rawValue)]?.valid == true)
        }
    }

    func testPinchReturningToItsInitialScaleRestoresTheStartingSize() {
        let canvas = CGSize(width: 800, height: 400)
        let button = TouchButton.chat
        var config = TouchConfiguration()
        let initial = config.relativeSize(button, in: canvas)
        config.resize(button, factor: 100, in: canvas, from: initial)
        config.resize(button, factor: 50, in: canvas, from: initial)
        XCTAssertEqual(config.relativeSize(button, in: canvas).width,
                       initial.proportionalWidthRange.upperBound, accuracy: 0.00001)
        config.resize(button, factor: 1, in: canvas, from: initial)
        let final = config.relativeSize(button, in: canvas)
        XCTAssertEqual(final.width, initial.width, accuracy: 0.00001)
        XCTAssertEqual(final.height, initial.height, accuracy: 0.00001)
    }

    func testSizingOpacityAndForwardLockRoundTrip() throws {
        let model = TouchPreferences(defaults:defaults)
        var config = model.configuration
        XCTAssertTrue(config.joystick)
        let size = CGSize(width:650,height:300)
        let original = config.rect(.joystick,in:size)
        config.resize(.joystick,factor:1.5,in:size)
        config.opacities["12"] = 0.35
        config.forwardLock = true
        model.save(config)
        let loaded = TouchPreferences(defaults:defaults).configuration
        XCTAssertEqual(loaded,config)
        let rect = loaded.rect(.joystick,in:size)
        XCTAssertEqual(rect.width,original.width*1.5,accuracy:0.0001)
        XCTAssertEqual(rect.width,rect.height)
        XCTAssertGreaterThanOrEqual(rect.minX,0)
        XCTAssertLessThanOrEqual(rect.maxY,size.height)
        let wire = loaded.snapshot()
        XCTAssertEqual(wire.0.count,100)
        XCTAssertEqual(wire.0[64],Float(rect.width / 300),accuracy:0.0001)
        XCTAssertEqual(wire.0[92],0.35,accuracy:0.0001)
        XCTAssertEqual(wire.1 & 32,32)
        for b in TouchButton.allCases { XCTAssertEqual(b.id,b.rawValue) }
        XCTAssertEqual(TouchButton.joystick.rawValue,12)
        XCTAssertEqual(TouchButton.playerList.rawValue,19)
    }
    func testRuntimeSlotStaysReservedButPauseIsTheOnlyMenuButton() {
        let config = TouchConfiguration()
        XCTAssertEqual(TouchButton.runtime.rawValue, 17)
        XCTAssertEqual(TouchButton.debug.rawValue, 18)
        XCTAssertFalse(config.buttons.contains(.runtime))
        XCTAssertTrue(config.buttons.contains(.pause))
        XCTAssertEqual(config.snapshot().0.count, 100)
    }
    func testV2SnapshotCapacityAndLegacyLimit() {
        var values = [Float](repeating:123,count:102)
        values.withUnsafeMutableBufferPointer { b in
            XCTAssertEqual(horizonTouchSnapshotV2(b.baseAddress,99),0)
            XCTAssertTrue(b.allSatisfy { $0 == 123 })
            _ = horizonTouchSnapshotV2(b.baseAddress,100)
        }
        XCTAssertEqual(values[100],123); XCTAssertEqual(values[101],123)
        XCTAssertEqual(horizonTouchSnapshotV2(nil,100),0)
        values = [Float](repeating:123,count:102)
        values.withUnsafeMutableBufferPointer { _ = horizonTouchPreferences($0.baseAddress,100) }
        XCTAssertTrue(values[26...].allSatisfy { $0 == 123 })
    }
    func testCExportWritesOnlyItsDocumentedCapacity() {
        var values = [Float](repeating: 123, count: 22)
        values.withUnsafeMutableBufferPointer { buffer in
            XCTAssertEqual(horizonTouchPreferences(buffer.baseAddress, 19), 0)
            XCTAssertTrue(buffer.allSatisfy { $0 == 123 })
            _ = horizonTouchPreferences(buffer.baseAddress, 20)
        }
        XCTAssertEqual(values[20], 123); XCTAssertEqual(values[21], 123)
        XCTAssertEqual(horizonTouchPreferences(nil, 20), 0)
    }
    func testHotbarSelectorUsesPhysicalIntegerCenterAndTwentyGUIRows() {
        let pixels = CGSize(width: 2420, height: 1668)
        XCTAssertEqual(TouchHotbar.slot(point: CGPoint(x: 670, y: 1547.999), pixels: pixels, scale: 6), -1)
        XCTAssertEqual(TouchHotbar.slot(point: CGPoint(x: 670, y: 1548), pixels: pixels, scale: 6), 0)
        XCTAssertEqual(TouchHotbar.slot(point: CGPoint(x: 670, y: 1667.999), pixels: pixels, scale: 6), 0)
        XCTAssertEqual(TouchHotbar.slot(point: CGPoint(x: 670, y: 1668), pixels: pixels, scale: 6), -1)
        XCTAssertEqual(TouchHotbar.slot(point: CGPoint(x: 670, y: 1700), pixels: pixels, scale: 6), -1)
        XCTAssertEqual(TouchHotbar.slot(point: CGPoint(x: 669.999, y: 1600), pixels: pixels, scale: 6), -1)
        XCTAssertEqual(TouchHotbar.slot(point: CGPoint(x: 1749.999, y: 1600), pixels: pixels, scale: 6), 8)
        XCTAssertEqual(TouchHotbar.slot(point: CGPoint(x: 1750, y: 1600), pixels: pixels, scale: 6), -1)
        for slot in 0..<9 {
            XCTAssertEqual(TouchHotbar.slot(point: CGPoint(x: 670 + slot * 120 + 60, y: 1600), pixels: pixels, scale: 6), slot)
        }
        XCTAssertEqual(TouchHotbar.slot(point: CGPoint(x: 1210, y: 1600), pixels: pixels, scale: .nan), -1)
    }
    func testNativeGestureQueueCapacityAndContextBoundary() {
        let view = UIView(frame: CGRect(x:0,y:0,width:800,height:400))
        let bridge = TouchGestureBridge.shared
        bridge.attach(to:view)
        _ = bridge.poll(screen:9001,flags:0,width:400,height:200,scale:2,capacity:64)
        let hold = FixedGestureHold(); hold.phase = .began
        _ = bridge.perform(NSSelectorFromString("keyboardHeld:"), with:hold)
        var bytes = [Double](repeating:123,count:7)
        bytes.withUnsafeMutableBufferPointer { buffer in
            XCTAssertEqual(horizonTouchGestures(9001,0,400,200,2,buffer.baseAddress,5),0)
            XCTAssertTrue(buffer.allSatisfy{$0 == 123})
            XCTAssertEqual(horizonTouchGestures(9001,0,400,200,2,buffer.baseAddress,6),1)
        }
        XCTAssertEqual(Array(bytes.prefix(5)),[5,1,0,0,-1]);XCTAssertGreaterThanOrEqual(bytes[5],0)
        XCTAssertEqual(bytes[6],123)
        _ = bridge.perform(NSSelectorFromString("keyboardHeld:"), with:hold)
        XCTAssertTrue(bridge.poll(screen:9002,flags:0,width:400,height:200,scale:2,capacity:64).isEmpty,
                      "Old-screen gestures must never reach a new screen")
        _ = bridge.perform(NSSelectorFromString("keyboardHeld:"), with:hold)
        XCTAssertTrue(bridge.poll(screen:9002,flags:4,width:0,height:0,scale:1,capacity:64).isEmpty)
        _ = bridge.perform(NSSelectorFromString("keyboardHeld:"), with:hold)
        XCTAssertEqual(bridge.poll(screen:9002,flags:0,width:400,height:200,scale:2,capacity:64).count,1,
                       "Discarding stale input must not disable recognition for the next touch")
        _ = bridge.poll(screen:9002,flags:2,width:400,height:200,scale:2,capacity:64)
    }
    func testGestureSettingsPersistWithoutChangingOldLayoutDefaults() throws {
        let old = try JSONDecoder().decode(TouchConfiguration.self,from:Data(#"{"centres":{}}"#.utf8))
        XCTAssertTrue(old.mouseGestures);XCTAssertTrue(old.hotbarGestures)
        let model=TouchPreferences(defaults:defaults)
        var value=old;value.mouseGestures=false;value.hotbarGestures=false;model.save(value)
        let loaded=TouchPreferences(defaults:defaults).configuration
        XCTAssertFalse(loaded.mouseGestures);XCTAssertFalse(loaded.hotbarGestures)
        XCTAssertEqual(loaded.snapshot().1 & 24,24)
    }

    @MainActor func testCustomHoldToggleAndCancellationEdges() {
        var definition=CustomTouchButton();definition.bindings=[-3,0,0,0]
        var edges:[Bool]=[]
        let held=CustomTouchView(definition){_,_,down,_ in edges.append(down)}
        held.sendActions(for:.touchDown);held.sendActions(for:.touchUpOutside)
        XCTAssertEqual(edges,[true,false]);edges=[]
        definition.toggle=true
        let toggle=CustomTouchView(definition){_,_,down,_ in edges.append(down)}
        toggle.sendActions(for:.touchDown);XCTAssertTrue(edges.isEmpty)
        toggle.sendActions(for:.touchUpInside);XCTAssertEqual(edges,[true]);XCTAssertTrue(toggle.isSelected)
        toggle.sendActions(for:.touchUpOutside);XCTAssertEqual(edges,[true])
        toggle.sendActions(for:.touchCancel);XCTAssertEqual(edges,[true,false]);XCTAssertFalse(toggle.isSelected)
    }
    @MainActor func testMenuCursorBindingOnlyFiresOnIntentionalActivation() throws {
        var definition = CustomTouchButton()
        definition.bindings = [-5, 340, 0, 0]
        var config = TouchConfiguration()
        config.customButtons = [definition]
        var cursorChanges = 0
        let bridge = TouchGestureBridge(configuration: { config }, toggleMenuCursor: { cursorChanges += 1 })
        let surface = UIView(frame: CGRect(x: 0, y: 0, width: 600, height: 300))
        bridge.attach(to: surface)
        func poll(screen: UInt64 = 1, flags: UInt32 = 0) -> [[Double]] {
            bridge.poll(screen: screen, flags: flags, width: 600, height: 300, scale: 1, capacity: 64)
        }
        _ = poll()
        let button = try XCTUnwrap(surface.subviews.compactMap { $0 as? CustomTouchView }.first)
        button.sendActions(for: .touchDown)
        XCTAssertEqual(bridge.customModifierBits, 1)
        button.sendActions(for: .touchCancel)
        XCTAssertEqual(bridge.customModifierBits, 0)
        XCTAssertEqual(poll().map { Array($0.prefix(3)) }, [[7, 1, 340], [7, 3, 340]])
        button.sendActions(for: .touchDown)
        button.sendActions(for: .touchUpOutside)
        button.sendActions(for: .touchDown)
        _ = poll(screen: 2)
        XCTAssertEqual(bridge.customModifierBits, 0)
        XCTAssertEqual(cursorChanges, 0)

        button.sendActions(for: .touchDown)
        button.sendActions(for: .touchUpInside)
        XCTAssertEqual(cursorChanges, 1)
        XCTAssertTrue(button.accessibilityActivate())
        XCTAssertEqual(cursorChanges, 2)
        _ = poll(screen: 3, flags: 1)
        XCTAssertTrue(button.accessibilityActivate())
        XCTAssertEqual(cursorChanges, 2)
    }

    @MainActor func testEditingAnOverlappingCustomControlPreservesItsStackingOrder() {
        var config = TouchConfiguration()
        let under = CustomTouchButton()
        let over = CustomTouchButton()
        config.customButtons = [under, over]
        let surface = UIView(frame: CGRect(x: 0, y: 0, width: 640, height: 480))
        let bridge = TouchGestureBridge(configuration: { config })
        bridge.attach(to: surface)

        func controlOrder() -> [UUID] {
            _ = bridge.poll(screen: 1, flags: 1, width: 640, height: 480, scale: 1, capacity: 32)
            return surface.subviews.compactMap { ($0 as? CustomTouchView)?.definition.id }
        }

        XCTAssertEqual(controlOrder(), [under.id, over.id])
        config.customButtons[0].name = "Edited"
        XCTAssertEqual(controlOrder(), [under.id, over.id])
    }

    func testCustomControlsPersistBeyondSixAndKeepOrderedChords() throws {
        var config = TouchConfiguration()
        for index in 0..<40 {
            var button=CustomTouchButton();button.name="Action \(index)";button.bindings=[341,86,0,0]
            config.customButtons.append(button)
        }
        let model=TouchPreferences(defaults:defaults);model.save(config)
        let restored=TouchPreferences(defaults:defaults).configuration
        XCTAssertEqual(restored.customButtons.count,40)
        XCTAssertEqual(Set(restored.customButtons.map(\.id)).count,40)
        XCTAssertEqual(restored.customButtons[0].keys,[341,86])
        XCTAssertFalse(restored.customButtons[0].toggle)
        let old=try JSONDecoder().decode(TouchConfiguration.self,from:Data(#"{"centres":{}}"#.utf8))
        XCTAssertTrue(old.customButtons.isEmpty)
    }

    func testPartiallyDamagedLayoutRetainsReadableControlsAndPreferences() throws {
        var first = CustomTouchButton()
        first.name = "First"
        first.bindings = [340, 65, 0, 0]
        var second = CustomTouchButton()
        second.name = "Second"
        let encoder = JSONEncoder()
        let firstJSON = try XCTUnwrap(JSONSerialization.jsonObject(with: encoder.encode(first)) as? [String: Any])
        let secondJSON = try JSONSerialization.jsonObject(with: encoder.encode(second))
        var duplicate = firstJSON
        duplicate["name"] = "Duplicate"
        var damaged = secondJSON as? [String: Any] ?? [:]
        damaged["bindings"] = "not a key list"
        let stored: [String: Any] = [
            "customButtons": [firstJSON, damaged, duplicate, secondJSON],
            "centres": ["5": ["x": 0.7, "y": 0.3], "6": ["x": "invalid", "y": 0.5]],
            "sizes": "invalid",
            "opacities": ["5": 0.6, "6": "invalid"],
            "joystick": false,
            "virtualMouse": "invalid",
            "showControls": false,
            "hotbarGestures": false,
        ]
        defaults.set(try JSONSerialization.data(withJSONObject: stored), forKey: TouchPreferences.key)
        let preferences = TouchPreferences(defaults: defaults)
        let recovered = preferences.configuration
        XCTAssertEqual(recovered.customButtons, [first, second])
        XCTAssertEqual(recovered.centres, ["5": TouchCentre(x: 0.7, y: 0.3)])
        XCTAssertTrue(recovered.sizes.isEmpty)
        XCTAssertEqual(recovered.opacities, ["5": 0.6])
        XCTAssertFalse(recovered.joystick)
        XCTAssertTrue(recovered.virtualMouse)
        XCTAssertFalse(recovered.showControls)
        XCTAssertFalse(recovered.hotbarGestures)
        preferences.save(recovered)
        XCTAssertEqual(TouchPreferences(defaults: defaults).configuration, recovered)
    }

    func testCustomSharedHoldsReleaseOnlyTheirOwnInputs() {
        let holds=CustomTouchHolds(), a=UUID(), b=UUID()
        XCTAssertEqual(holds.set(a,keys:[341,86],down:true).map{$0.0},[341,86])
        let second = holds.set(b, keys: [341, 67], down: true)
        XCTAssertEqual(second.map { $0.0 }, [67])
        XCTAssertTrue(second.allSatisfy { $0.1 })
        let first=holds.set(a,keys:[341,86],down:false)
        XCTAssertEqual(first.map{$0.0},[86]);XCTAssertTrue(first.allSatisfy{!$0.1})
        XCTAssertTrue(holds.modifiers.contains(.control))
        XCTAssertEqual(holds.set(b,keys:[341,67],down:false).map{$0.0},[67,341])
        XCTAssertTrue(holds.modifiers.isEmpty)
    }

    @MainActor func testCustomChordAppliesModifiersBeforeKeysAndMouseButtons() throws {
        var definition = CustomTouchButton()
        definition.bindings = [86, -3, 341, 340]
        var config = TouchConfiguration()
        config.customButtons = [definition]
        let bridge = TouchGestureBridge(configuration: { config })
        let surface = UIView(frame: CGRect(x: 0, y: 0, width: 600, height: 300))
        bridge.attach(to: surface)
        func poll() -> [[Double]] {
            bridge.poll(screen: 1, flags: 0, width: 600, height: 300, scale: 1,
                        capacity: 64, withContext: true)
        }
        _ = poll()
        let button = try XCTUnwrap(surface.subviews.compactMap { $0 as? CustomTouchView }.first)
        button.sendActions(for: .touchDown)
        let pressed = poll()
        XCTAssertEqual(pressed.map { Array($0.prefix(3)) },
                       [[7, 1, 341], [7, 1, 340], [7, 1, 86], [7, 1, -3]])
        XCTAssertEqual(pressed.map { $0[8] }, [2, 3, 3, 3])

        button.sendActions(for: .touchUpInside)
        let released = poll()
        XCTAssertEqual(released.map { Array($0.prefix(3)) },
                       [[7, 3, 86], [7, 3, -3], [7, 3, 341], [7, 3, 340]])
        XCTAssertEqual(released.map { $0[8] }, [3, 3, 1, 0])
        XCTAssertEqual(bridge.customModifierBits, 0)
    }

    @MainActor func testHiddenOrRemovedCustomButtonCannotReactivateAKey() throws {
        var definition = CustomTouchButton()
        definition.bindings = [340, 0, 0, 0]
        definition.toggle = true
        var config = TouchConfiguration()
        config.customButtons = [definition]
        let bridge = TouchGestureBridge(configuration: { config })
        let surface = UIView(frame: CGRect(x: 0, y: 0, width: 600, height: 300))
        bridge.attach(to: surface)
        func poll(flags: UInt32 = 0) -> [[Double]] {
            bridge.poll(screen: 1, flags: flags, width: 600, height: 300, scale: 1, capacity: 64)
        }
        _ = poll()
        let button = try XCTUnwrap(surface.subviews.compactMap { $0 as? CustomTouchView }.first)
        XCTAssertTrue(button.accessibilityActivate())
        XCTAssertEqual(poll().map { Array($0.prefix(3)) }, [[7, 1, 340]])

        XCTAssertEqual(poll(flags: 16).map { Array($0.prefix(3)) }, [[7, 3, 340]])
        XCTAssertTrue(button.isHidden)
        XCTAssertFalse(button.accessibilityActivate())
        button.sendActions(for: .touchDown)
        button.sendActions(for: .touchUpInside)
        XCTAssertTrue(poll(flags: 16).isEmpty)
        XCTAssertEqual(bridge.customModifierBits, 0)

        XCTAssertTrue(poll().isEmpty)
        XCTAssertFalse(button.isHidden)
        XCTAssertTrue(button.accessibilityActivate())
        XCTAssertEqual(poll().map { Array($0.prefix(3)) }, [[7, 1, 340]])
        config.customButtons.removeAll()
        XCTAssertEqual(poll().map { Array($0.prefix(3)) }, [[7, 3, 340]])
        XCTAssertNil(button.superview)
        XCTAssertFalse(button.accessibilityActivate())
        XCTAssertTrue(poll().isEmpty)
        XCTAssertEqual(bridge.customModifierBits, 0)
    }

    @MainActor func testCoveringTheGameClearsLatchedInputBeforeTheSameScreenResumes() throws {
        var definition = CustomTouchButton()
        definition.bindings = [340, 0, 0, 0]
        definition.toggle = true
        var config = TouchConfiguration()
        config.customButtons = [definition]
        let bridge = TouchGestureBridge(configuration: { config })
        let surface = UIView(frame: CGRect(x: 0, y: 0, width: 600, height: 300))
        bridge.attach(to: surface)
        func poll(flags: UInt32 = 0, capacity: Int = 64) -> [[Double]] {
            bridge.poll(screen: 1, flags: flags, width: 600, height: 300, scale: 1,
                        capacity: capacity, withContext: true)
        }
        _ = poll()
        let button = try XCTUnwrap(surface.subviews.compactMap { $0 as? CustomTouchView }.first)
        button.sendActions(for: .touchDown)
        button.sendActions(for: .touchUpInside)
        XCTAssertEqual(poll().map { Array($0.prefix(3)) }, [[7, 1, 340]])
        XCTAssertEqual(bridge.customModifierBits, 1)

        bridge.inputContext = 99
        bridge.suspend()
        XCTAssertTrue(button.isHidden)
        XCTAssertFalse(button.isSelected)
        XCTAssertEqual(bridge.customModifierBits, 0)
        XCTAssertTrue(poll(flags: 2).isEmpty)
        XCTAssertTrue(poll(capacity: 0).isEmpty)
        XCTAssertFalse(button.isHidden)

        button.sendActions(for: .touchDown)
        button.sendActions(for: .touchUpInside)
        let events = poll()
        XCTAssertEqual(events.map { Array($0.prefix(3)) }, [[0, 4, 0], [7, 1, 340]])
        XCTAssertEqual(Array(events.first?.dropFirst(6) ?? []).prefix(2), [0, 0])
        XCTAssertTrue(poll().isEmpty)
    }

    @MainActor func testGestureQueueOverflowClearsLatchedModifiersAndDropsTheWholeChord() throws {
        var shift = CustomTouchButton()
        shift.bindings = [340, 0, 0, 0]
        shift.toggle = true
        var chord = CustomTouchButton()
        chord.bindings = [341, 86, 0, 0]
        var config = TouchConfiguration()
        config.customButtons = [shift, chord]
        let bridge = TouchGestureBridge(configuration: { config })
        let surface = UIView(frame: CGRect(x: 0, y: 0, width: 600, height: 300))
        bridge.attach(to: surface)
        func poll() -> [[Double]] {
            bridge.poll(screen: 1, flags: 0, width: 600, height: 300, scale: 1,
                        capacity: 128, withContext: true)
        }
        _ = poll()
        let buttons = surface.subviews.compactMap { $0 as? CustomTouchView }
        let shiftButton = try XCTUnwrap(buttons.first { $0.definition.id == shift.id })
        let chordButton = try XCTUnwrap(buttons.first { $0.definition.id == chord.id })
        shiftButton.sendActions(for: .touchDown)
        shiftButton.sendActions(for: .touchUpInside)
        XCTAssertEqual(poll().map { Array($0.prefix(3)) }, [[7, 1, 340]])
        XCTAssertTrue(shiftButton.isSelected)

        bridge.inputContext = 99
        for _ in 0..<32 {
            chordButton.sendActions(for: .touchDown)
            chordButton.sendActions(for: .touchUpInside)
        }
        shiftButton.sendActions(for: .touchDown)
        chordButton.sendActions(for: .touchDown)
        XCTAssertEqual(bridge.customModifierBits, 0)
        XCTAssertFalse(shiftButton.isSelected)
        let reset = poll()
        XCTAssertEqual(reset.map { Array($0.prefix(3)) }, [[0, 4, 0]])
        XCTAssertEqual(Array(reset.first?.dropFirst(6) ?? []).prefix(2), [0, 0])

        chordButton.sendActions(for: .touchUpInside)
        shiftButton.sendActions(for: .touchUpInside)
        XCTAssertTrue(poll().isEmpty)
        XCTAssertFalse(shiftButton.isSelected)
        chordButton.sendActions(for: .touchDown)
        chordButton.sendActions(for: .touchUpInside)
        XCTAssertEqual(poll().map { Array($0.prefix(3)) },
                       [[7, 1, 341], [7, 1, 86], [7, 3, 86], [7, 3, 341]])
    }

    @MainActor func testOneShotOnlyButtonNeverLatchesEvenWithAnOldToggleSetting() {
        var definition = CustomTouchButton()
        definition.bindings = [-7, 0, 0, 0]
        definition.toggle = true
        var activations = 0
        var holds = 0
        let button = CustomTouchView(definition, activate: { _ in activations += 1 }) { _, _, _, _ in
            holds += 1
        }
        button.sendActions(for: .touchDown)
        XCTAssertEqual(activations, 0)
        button.sendActions(for: .touchUpInside)
        XCTAssertFalse(button.isSelected)
        XCTAssertTrue(button.accessibilityActivate())
        XCTAssertFalse(button.isSelected)
        XCTAssertEqual(activations, 2)
        XCTAssertEqual(holds, 0)
    }

    @MainActor func testOneShotActionKeepsItsChordModifiersThroughActivation() throws {
        var definition = CustomTouchButton()
        definition.bindings = [341, -7, 0, 0]
        var config = TouchConfiguration()
        config.customButtons = [definition]
        let bridge = TouchGestureBridge(configuration: { config })
        let surface = UIView(frame: CGRect(x: 0, y: 0, width: 600, height: 300))
        bridge.attach(to: surface)
        func poll() -> [[Double]] {
            bridge.poll(screen: 1, flags: 0, width: 600, height: 300, scale: 1,
                        capacity: 64, withContext: true)
        }
        _ = poll()
        let button = try XCTUnwrap(surface.subviews.compactMap { $0 as? CustomTouchView }.first)
        button.sendActions(for: .touchDown)
        button.sendActions(for: .touchUpInside)
        let events = poll()
        XCTAssertEqual(events.map { Array($0.prefix(3)) }, [[7, 1, 341], [7, 3, -7], [7, 3, 341]])
        XCTAssertEqual(events.map { $0[8] }, [2, 2, 0])
        XCTAssertEqual(bridge.customModifierBits, 0)

        XCTAssertTrue(button.accessibilityActivate())
        let accessible = poll()
        XCTAssertEqual(accessible.map { Array($0.prefix(3)) }, events.map { Array($0.prefix(3)) })
        XCTAssertEqual(accessible.map { $0[8] }, [2, 2, 0])
    }

    @MainActor func testOneShotCustomActionsCannotFireFromCancelledOrOutsideTouches() throws {
        var definition = CustomTouchButton()
        definition.bindings = [-1, -2, -7, -8]
        var config = TouchConfiguration()
        config.customButtons = [definition]
        let bridge = TouchGestureBridge(configuration: { config })
        let surface = UIView(frame: CGRect(x: 0, y: 0, width: 600, height: 300))
        bridge.attach(to: surface)
        func poll() -> [[Double]] {
            bridge.poll(screen: 1, flags: 0, width: 600, height: 300, scale: 1, capacity: 64)
        }
        _ = poll()
        let button = try XCTUnwrap(surface.subviews.compactMap { $0 as? CustomTouchView }.first)
        for event: UIControl.Event in [.touchCancel, .touchUpOutside] {
            button.sendActions(for: .touchDown)
            button.sendActions(for: event)
            XCTAssertTrue(poll().isEmpty)
        }
        button.sendActions(for: .touchDown)
        button.sendActions(for: .touchUpInside)
        XCTAssertEqual(poll().map { Array($0.prefix(3)) },
                       [[7, 3, -1], [7, 3, -2], [7, 3, -7], [7, 3, -8]])
    }

    func testCustomRectUsesTheSameNormalizedCanvasAfterRotation() {
        var button=CustomTouchButton();button.centre=TouchCentre(x:0.5,y:0.5)
        for size in [CGSize(width:600,height:300),CGSize(width:300,height:600)] {
            let r=button.rect(in:size)
            XCTAssertEqual(r.midX,size.width/2);XCTAssertEqual(r.midY,size.height/2)
            XCTAssertEqual(r.width,45);XCTAssertEqual(r.height,36)
        }
    }

    func testCustomDragClampsTheStoredCentreUsingTheActualButtonSize() {
        let canvas = CGSize(width: 600, height: 300)
        var button = CustomTouchButton()
        button.size = TouchSize(width: 0.4, height: 0.25)
        button.opacity = 0.3
        let moved = button.moved(by: CGSize(width: 1_000, height: -1_000), in: canvas)
        XCTAssertEqual(moved.rect(in: canvas).maxX, canvas.width, accuracy: 0.001)
        XCTAssertEqual(moved.rect(in: canvas).minY, 0, accuracy: 0.001)
        XCTAssertEqual(moved.centre.x, 0.9, accuracy: 0.001)
        XCTAssertEqual(moved.centre.y, 0.125, accuracy: 0.001)
        XCTAssertEqual(moved.effectiveOpacity, 0.3)
        let back = moved.moved(by: CGSize(width: -30, height: 10), in: canvas)
        XCTAssertEqual(back.rect(in: canvas).maxX, canvas.width - 30, accuracy: 0.001)
        XCTAssertEqual(back.rect(in: canvas).minY, 10, accuracy: 0.001)
        XCTAssertEqual(back.moved(by: CGSize(width: CGFloat.nan, height: 0), in: canvas), back)
    }

}

private final class FixedGestureHold: UILongPressGestureRecognizer {
    var phase: UIGestureRecognizer.State = .began
    var point = CGPoint.zero
    override var state: UIGestureRecognizer.State { get { phase } set { phase = newValue } }
    override func location(in view: UIView?) -> CGPoint { point }
}

private final class PositionedTouch: UITouch {
    private let point: CGPoint
    init(_ point: CGPoint) { self.point = point; super.init() }
    override func location(in view: UIView?) -> CGPoint { point }
}
