import SwiftUI
import UIKit

/// Native control preferences and full-screen layout editor.
/// Input comparison and reference pins: docs/ios/pojav-input-parity.md.
/// IDs/order are the version-one ABI in touch.rs and horizon-ios-launcher.
enum TouchButton: Int, CaseIterable, Codable, Identifiable {
    case forward, back, left, right, sneak, jump, perspective, chat, pause, inventory, leftClick, rightClick, joystick, sprint, middleClick, more, hide, runtime, debug, playerList
    var id: Int { rawValue }
    var title: String {
        switch self {
        case .forward: return "Forward"
        case .back: return "Back"
        case .left: return "Left"
        case .right: return "Right"
        case .sneak: return "Sneak"
        case .jump: return "Jump"
        case .perspective: return "View"
        case .chat: return "Chat"
        case .pause: return "Pause"
        case .inventory: return "Inventory"
        case .leftClick: return "Left click"
        case .rightClick: return "Right click"
        case .joystick: return "Joystick"
        case .sprint: return "Sprint"
        case .middleClick: return "Middle click"
        case .more: return "More"
        case .hide: return "Hide GUI"
        case .runtime: return "Runtime Menu"
        case .debug: return "Debug Menu"
        case .playerList: return "Player List"
        }
    }
    var symbol: String {
        switch self {
        case .forward: return "arrow.up"
        case .back: return "arrow.down"
        case .left: return "arrow.left"
        case .right: return "arrow.right"
        case .sneak: return "figure.walk"
        case .jump: return "arrow.up.to.line"
        case .perspective: return "camera.rotate"
        case .chat: return "text.bubble"
        case .pause: return "pause"
        case .inventory: return "ellipsis"
        case .leftClick: return "computermouse"
        case .rightClick: return "computermouse.fill"
        case .joystick: return "circle.fill"
        case .sprint: return "figure.run"
        case .middleClick: return "cursorarrow.click"
        case .more: return "ellipsis"
        case .hide: return "eye.slash"
        case .runtime: return "line.3.horizontal"
        case .debug: return "ladybug"
        case .playerList: return "person.2"
        }
    }
    // GUI pixels, matching touch::default_layout. Hotbar is the game's HUD,
    // not an independently movable touch button.
    func rect(in size: CGSize) -> CGRect {
        let w = size.width, h = size.height
        let margin = min(14, min(w, h) / 4)
        let bottom = min(16, h / 4)
        let d = min(48, max(26, h * 0.14), (w - margin) / 3, (h - margin - bottom) / 3)
        let cx = margin + d, cy = h - margin - 2 * d - bottom
        let unit = min(w,h) / 768, s = 69.6 * unit, stick = 204.8 * unit
        let right = 0.94878495 * w - s, gap = 2 * unit
        switch self {
        case .forward: return CGRect(x:cx,y:cy-d,width:d,height:d)
        case .back: return CGRect(x:cx,y:cy+d,width:d,height:d)
        case .left: return CGRect(x:cx-d,y:cy,width:d,height:d)
        case .right: return CGRect(x:cx+d,y:cy,width:d,height:d)
        case .sneak: return CGRect(x:right,y:0.31131014*h,width:s,height:s)
        case .jump: return CGRect(x:right,y:0.31131014*h+s+gap,width:s,height:s)
        case .sprint: return CGRect(x:right-s-gap,y:0.31131014*h,width:s,height:s)
        case .leftClick: return CGRect(x:0.7756397*w-s,y:0.8224885*h-s,width:s,height:s)
        case .rightClick: return CGRect(x:0.8756154*w-s,y:0.8178498*h-s,width:s,height:s)
        case .middleClick: return CGRect(x:0.83548373*w-s,y:0.67798775*h-s,width:s,height:s)
        case .joystick: return CGRect(x:0.02326631*w,y:min(0.9753418*h-stick,h > w ? h-min(34,h/2)-stick : h),width:stick,height:stick)
        case .perspective: return CGRect(x:98*unit,y:h>w ? 42*unit : 0,width:96*unit,height:40*unit)
        case .chat: return CGRect(x:0,y:42*unit,width:96*unit,height:40*unit)
        case .pause: return CGRect(x:w-3*106.8*unit,y:0,width:104.8*unit,height:39.2*unit)
        case .inventory: return CGRect(x:w-3*106.8*unit-102*unit,y:0,width:100*unit,height:40*unit)
        case .more: return CGRect(x:0,y:0,width:96*unit,height:40*unit)
        case .debug: return CGRect(x:294*unit,y:h>w ? 42*unit : 0,width:96*unit,height:40*unit)
        case .playerList: return CGRect(x:196*unit,y:h>w ? 42*unit : 0,width:96*unit,height:40*unit)
        case .hide: return CGRect(x:w-69.6*unit,y:0,width:69.6*unit,height:40*unit)
        case .runtime: return CGRect(x:w-176.4*unit,y:0,width:104.8*unit,height:39.2*unit)
        }
    }
}

struct TouchCentre: Codable, Equatable {
    var x: Double
    var y: Double
    var valid: Bool { x.isFinite && y.isFinite && (0...1).contains(x) && (0...1).contains(y) }
}

struct TouchSize: Codable, Equatable {
    var width: Double
    var height: Double
    var valid: Bool { width.isFinite && height.isFinite && (0.02...0.8).contains(width) && (0.02...0.8).contains(height) }

    var proportionalWidthRange: ClosedRange<Double> {
        let aspect = width / height
        let lower = max(0.02, min(0.8, 0.02 * aspect))
        let upper = max(lower, min(0.8, 0.8 * aspect))
        return lower...upper
    }
}

/// Decode entries independently so a damaged control does not erase its neighbours.
private struct SavedTouchValue<Value: Decodable>: Decodable {
    let value: Value?
    init(from decoder: Decoder) {
        value = try? Value(from: decoder)
    }
}

struct TouchConfiguration: Codable, Equatable {
    var centres: [String: TouchCentre] = [:]
    var customButtons: [CustomTouchButton] = []
    var sizes: [String: TouchSize] = [:]
    var opacities: [String: Double] = [:]
    var forwardLock = false
    var showControls = true
    var virtualMouse = true
    var joystick = true
    var mouseGestures = true
    var hotbarGestures = true
    var slideableHotbar = false
    var holdDuration = 0.4
    enum CodingKeys: String, CodingKey { case customButtons, centres, sizes, opacities, forwardLock, showControls, virtualMouse, joystick, mouseGestures, hotbarGestures, slideableHotbar, holdDuration }
    init() {}
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        func field<T: Decodable>(_ type: T.Type, _ key: CodingKeys) -> T? {
            try? c.decode(type, forKey: key)
        }
        let buttons = field([SavedTouchValue<CustomTouchButton>].self, .customButtons) ?? []
        var ids = Set<UUID>()
        customButtons = buttons.compactMap(\.value).filter { ids.insert($0.id).inserted }
        centres = field([String: SavedTouchValue<TouchCentre>].self, .centres)?
            .compactMapValues(\.value) ?? [:]
        sizes = field([String: SavedTouchValue<TouchSize>].self, .sizes)?
            .compactMapValues(\.value) ?? [:]
        opacities = field([String: SavedTouchValue<Double>].self, .opacities)?
            .compactMapValues(\.value) ?? [:]
        forwardLock = field(Bool.self, .forwardLock) ?? false
        showControls = field(Bool.self, .showControls) ?? true
        virtualMouse = field(Bool.self, .virtualMouse) ?? true
        if !c.contains(.joystick) {
            // Layouts saved before joystick mode used the directional buttons.
            joystick = false
        } else if let saved = field(Bool.self, .joystick) {
            joystick = saved
        }
        mouseGestures = field(Bool.self, .mouseGestures) ?? true
        hotbarGestures = field(Bool.self, .hotbarGestures) ?? true
        slideableHotbar = field(Bool.self, .slideableHotbar) ?? false
        let duration = field(Double.self, .holdDuration) ?? 0.4
        holdDuration = duration.isFinite ? min(2, max(0.1, duration)) : 0.4
    }
    var buttons: [TouchButton] {
        TouchButton.allCases.filter { button in
            // Keep the reserved ID for saved layouts, but Pause already owns Escape.
            if button == .runtime { return false }
            if button == .joystick { return joystick }
            if [.forward, .back, .left, .right].contains(button) { return !joystick }
            return true
        }
    }

    func rect(_ button: TouchButton, in size: CGSize) -> CGRect {
        var r = button.rect(in: size)
        if let value = sizes[String(button.rawValue)], value.valid {
            let unit = min(size.width,size.height)
            let centre = CGPoint(x:r.midX,y:r.midY)
            r.size = CGSize(width:min(size.width,value.width*unit),height:min(size.height,(button == .joystick ? value.width : value.height)*unit))
            r.origin = CGPoint(x:centre.x-r.width/2,y:centre.y-r.height/2)
        }
        if let c = centres[String(button.rawValue)], c.valid {
            r.origin.x = max(0, min(size.width - r.width, c.x * size.width - r.width / 2))
            r.origin.y = max(0, min(size.height - r.height, c.y * size.height - r.height / 2))
        }
        r.origin.x = max(0,min(size.width-r.width,r.origin.x))
        r.origin.y = max(0,min(size.height-r.height,r.origin.y))
        return r
    }
    func opacity(_ button: TouchButton) -> Double {
        guard let value = opacities[String(button.rawValue)], value.isFinite else { return 1 }
        return min(1,max(0.1,value))
    }
    func relativeSize(_ button: TouchButton, in canvas: CGSize) -> TouchSize {
        if let saved = sizes[String(button.rawValue)], saved.valid {
            return TouchSize(width: saved.width, height: button == .joystick ? saved.width : saved.height)
        }
        let unit = min(canvas.width, canvas.height)
        guard unit.isFinite, unit > 0 else { return TouchSize(width: 0.02, height: 0.02) }
        let rect = button.rect(in: canvas)
        return TouchSize(width: rect.width / unit, height: rect.height / unit)
    }

    mutating func resize(_ button: TouchButton, factor: Double, in canvas: CGSize,
                         from initial: TouchSize? = nil) {
        guard factor.isFinite, factor > 0, min(canvas.width, canvas.height) > 0 else { return }
        let original = initial ?? relativeSize(button, in: canvas)
        let range = original.proportionalWidthRange
        let width = min(range.upperBound, max(range.lowerBound, original.width * factor))
        // Limit the proportional size first, then absorb floating-point roundoff.
        let height = min(0.8, max(0.02, width / (original.width / original.height)))
        sizes[String(button.rawValue)] = TouchSize(width: width, height: height)
        // Keep the chosen anchor. rect() fits it to the current canvas
        // without saving a temporary shift away from an edge.
    }
    mutating func move(_ button: TouchButton, to point: CGPoint, in size: CGSize) {
        guard size.width > 0, size.height > 0 else { return }
        let r = rect(button, in: size)
        let x = max(r.width / 2, min(size.width - r.width / 2, point.x))
        let y = max(r.height / 2, min(size.height - r.height / 2, point.y))
        centres[String(button.rawValue)] = TouchCentre(x: x / size.width, y: y / size.height)
    }

    mutating func resetLayout() {
        centres.removeAll()
        sizes.removeAll()
        opacities.removeAll()
        guard !customButtons.isEmpty else { return }

        let appearance = CustomTouchButton()
        // Custom actions have no built-in slots. Spread them out in an even
        // grid so resetting several buttons does not stack them at one point.
        let columns = Int(ceil(sqrt(Double(customButtons.count))))
        let rows = (customButtons.count + columns - 1) / columns
        let horizontalSpan = 0.7
        let verticalSpan = 0.6
        // Centres are relative to the full canvas, while button size is
        // relative to its short side. Fit the tighter spacing on either axis.
        let scale = min(1, 0.9 * horizontalSpan / Double(columns) / appearance.size.width,
                        0.9 * verticalSpan / Double(rows) / appearance.size.height)
        let size = TouchSize(width: max(0.02, appearance.size.width * scale),
                             height: max(0.02, appearance.size.height * scale))
        for index in customButtons.indices {
            let row = index / columns
            let countInRow = min(columns, customButtons.count - row * columns)
            let column = index % columns
            customButtons[index].centre = TouchCentre(
                x: 0.15 + horizontalSpan * (Double(column) + 0.5) / Double(countInRow),
                y: rows == 1 ? 0.75 : 0.25 + verticalSpan * (Double(row) + 0.5) / Double(rows)
            )
            customButtons[index].size = size
            customButtons[index].opacity = appearance.opacity
        }
    }

    func snapshot() -> ([Float], UInt32) {
        let points = TouchButton.allCases.flatMap { b -> [Float] in
            guard let c = centres[String(b.rawValue)], c.valid else { return [.nan, .nan] }
            return [Float(c.x), Float(c.y)]
        }
        let dimensions = TouchButton.allCases.flatMap { b -> [Float] in
            guard let value = sizes[String(b.rawValue)], value.valid else { return [.nan,.nan] }
            return [Float(value.width),Float(b == .joystick ? value.width : value.height)]
        }
        let alpha = TouchButton.allCases.map { Float(opacity($0)) }
        return (points + dimensions + alpha, (forwardLock ? 32 : 0) | (showControls ? 0 : 1) | (virtualMouse ? 2 : 0) | (joystick ? 4 : 0) | (mouseGestures ? 0 : 8) | (hotbarGestures ? 0 : 16))
    }
}

/// The synchronous C reader may be called from a different thread. Only plain
/// values are read under this lock; there are no UIKit calls in the render loop.
final class TouchPreferences: ObservableObject {
    static let shared = TouchPreferences()
    static let key = "touchControls.v1"
    @Published private(set) var configuration: TouchConfiguration
    private let defaults: UserDefaults
    private let lock = NSLock()
    private var cached: TouchConfiguration

    init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
        let config = defaults.data(forKey: Self.key).flatMap { try? JSONDecoder().decode(TouchConfiguration.self, from: $0) } ?? TouchConfiguration()
        configuration = config; cached = config
    }
    func save(_ value: TouchConfiguration) {
        guard let data = try? JSONEncoder().encode(value) else { return }
        defaults.set(data, forKey: Self.key)
        lock.lock(); cached = value; lock.unlock()
        configuration = value
    }
    func snapshot() -> ([Float], UInt32) {
        lock.lock(); defer { lock.unlock() }
        return cached.snapshot()
    }
}

@_cdecl("horizon_touch_preferences_v1")
public func horizonTouchPreferences(_ buffer: UnsafeMutablePointer<Float>?, _ capacity: UInt32) -> UInt32 {
    guard let buffer, capacity >= 20 else { return 0 }
    let (values, flags) = TouchPreferences.shared.snapshot()
    for i in 0..<min(Int(capacity), 26) { buffer[i] = values[i] }
    return flags
}

@_cdecl("horizon_touch_snapshot_v2")
public func horizonTouchSnapshotV2(_ buffer: UnsafeMutablePointer<Float>?, _ capacity: UInt32) -> UInt32 {
    guard let buffer, capacity >= 100 else { return 0 }
    let (values, flags) = TouchPreferences.shared.snapshot()
    for i in 0..<100 { buffer[i] = values[i] }
    return flags
}

struct TouchControlsSettings: View {
    @ObservedObject private var preferences = TouchPreferences.shared
    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            SectionHeader(title: "Touch controls")
            SecondaryButton(title: "Edit button layout", icon: "hand.draw") { presentTouchEditor() }
                .accessibilityIdentifier("touch-edit-layout")
            Toggle("Show touch controls", isOn: Binding(get: { preferences.configuration.showControls }, set: { value in
                var c = preferences.configuration; c.showControls = value; preferences.save(c)
            })).accessibilityIdentifier("touch-show-controls")
            Picker("Menu input", selection: Binding(get: { preferences.configuration.virtualMouse }, set: { value in
                var c = preferences.configuration; c.virtualMouse = value; preferences.save(c)
            })) { Text("Mouse cursor").tag(true); Text("Direct touch").tag(false) }
                .accessibilityIdentifier("touch-menu-mode")
            Picker("Movement", selection: Binding(get: { preferences.configuration.joystick }, set: { value in
                var c = preferences.configuration; c.joystick = value; preferences.save(c)
            })) { Text("D-pad").tag(false); Text("Joystick").tag(true) }
                .accessibilityIdentifier("touch-movement-mode")
            Toggle("Forward lock", isOn: Binding(get: { preferences.configuration.forwardLock }, set: { value in
                var c = preferences.configuration; c.forwardLock = value; preferences.save(c)
            })).disabled(!preferences.configuration.joystick)
            Toggle("Tap and hold gestures", isOn: Binding(get: { preferences.configuration.mouseGestures }, set: { value in
                var c = preferences.configuration; c.mouseGestures = value; preferences.save(c)
            }))
            Toggle("Hotbar gestures", isOn: Binding(get: { preferences.configuration.hotbarGestures }, set: { value in
                var c = preferences.configuration; c.hotbarGestures = value; preferences.save(c)
            }))
            Toggle("Slide across hotbar slots", isOn: Binding(get: { preferences.configuration.slideableHotbar }, set: { value in
                var c = preferences.configuration; c.slideableHotbar = value; preferences.save(c)
            }))
            VStack(alignment: .leading) {
                Text("Hold duration: \(Int(preferences.configuration.holdDuration * 1000)) ms")
                Slider(value: Binding(get: { preferences.configuration.holdDuration }, set: { value in
                    var c = preferences.configuration; c.holdDuration = value; preferences.save(c)
                }), in: 0.1...2, step: 0.05)
            }
            Text("Controls remain visible during screen recording. For a clean system screenshot, turn off Show touch controls first; the touch areas remain active.")
                .font(.footnote).foregroundStyle(.secondary)
        }
    }
}

@MainActor
func presentTouchEditor() {
    guard let presenter = gameViewController() else { return }
    let host = TouchEditorController(rootView: AnyView(EmptyView()))
    host.modalPresentationStyle = .fullScreen
    let previous = presenter.view.window?.windowScene?.interfaceOrientation ?? .portrait
    let restore: UIInterfaceOrientationMask
    switch previous {
    case .landscapeLeft: restore = .landscapeLeft
    case .landscapeRight: restore = .landscapeRight
    case .portraitUpsideDown: restore = .portraitUpsideDown
    default: restore = .portrait
    }
    host.rootView = AnyView(TouchLayoutEditor(initial: TouchPreferences.shared.configuration) { [weak host] saved in
        guard let host else { return }
        if let saved { TouchPreferences.shared.save(saved) }
        let scene = host.view.window?.windowScene
        host.dismiss(animated: true) {
            turn(scene, to: UIDevice.current.userInterfaceIdiom == .pad ? restore : .portrait)
        }
    })
    presenter.present(host, animated: true)
}

private final class TouchEditorController: UIHostingController<AnyView> {
    override var supportedInterfaceOrientations: UIInterfaceOrientationMask { .landscape }
    override var preferredInterfaceOrientationForPresentation: UIInterfaceOrientation { .landscapeRight }
    override var prefersStatusBarHidden: Bool { true }
    override func viewDidAppear(_ animated: Bool) {
        super.viewDidAppear(animated)
        turn(view.window?.windowScene, to: .landscape)
    }
}

private struct TouchLayoutEditor: View {
    @State private var draft: TouchConfiguration
    @State private var pinchStart: TouchSize?
    @GestureState private var isPinching = false
    @State private var dragStart: CGPoint?
    @State private var selected: TouchButton?
    @State private var confirmReset = false
    @State private var editingCustom: UUID?
    let finish: (TouchConfiguration?) -> Void
    init(initial: TouchConfiguration, finish: @escaping (TouchConfiguration?) -> Void) {
        _draft = State(initialValue: initial); self.finish = finish
    }
    @State private var toolbarCentre: CGPoint?
    @State private var toolbarStart: CGPoint?
    @Environment(\.displayScale) private var displayScale
    private let guiOptions = TouchEditorGeometry.savedOptions()
    var body: some View {
        GeometryReader { geometry in
            let width = geometry.size.width
            let height = geometry.size.height
            let metrics = TouchEditorGeometry(size: geometry.size, displayScale: displayScale,
                                              limit: guiOptions.limit, forceUnicode: guiOptions.forceUnicode)
            let gui = metrics.gui
            let scale = metrics.pointsPerUnit
            let toolbar = TouchEditorToolbarGeometry(canvas: geometry.size)
            ZStack(alignment: .topLeading) {
                Color.black
                Rectangle().stroke(Color.white.opacity(0.2), lineWidth: 1)
                    .frame(width: 182 * scale, height: 22 * scale)
                    .position(x: (gui.width / 2) * scale, y: (gui.height - 11) * scale)
                    .allowsHitTesting(false)
                    ForEach(draft.buttons) { button in
                        let r = draft.rect(button, in: gui)
                        Text(button == .joystick ? "" : button.title)
                            .multilineTextAlignment(.center)
                            .font(.system(size: max(12, r.height * scale * 0.28), weight: .semibold))
                            .opacity(button == .joystick ? 0 : 1)
                            .frame(width: r.width * scale, height: r.height * scale)
                            .background {
                                if button == .joystick { Circle().fill(selected == button ? Color.accentColor.opacity(0.3) : Color.white.opacity(0.18)).frame(width:r.width * scale * 0.75,height:r.height * scale * 0.75) }
                                else { RoundedRectangle(cornerRadius: r.height * scale * 0.2).fill(selected == button ? Color.accentColor.opacity(0.45) : Color.white.opacity(0.18)) }
                            }
                            .overlay {
                                if button == .joystick {
                                    if selected == button { Circle().stroke(.white.opacity(0.6)).frame(width:r.width * scale * 0.75,height:r.height * scale * 0.75) }
                                    Circle().fill(.black)
                                        .frame(width: min(r.width,r.height) * scale * 0.25,
                                               height: min(r.width,r.height) * scale * 0.25)
                                }
                                else { RoundedRectangle(cornerRadius:r.height * scale * 0.2).stroke(.white.opacity(0.6)) }
                            }
                            .opacity(draft.opacity(button))
                            .clipShape(Rectangle())
                            .contentShape(Rectangle())
                            .position(x: r.midX * scale, y: r.midY * scale)
                            .zIndex(selected == button ? 1 : 0)
                            .accessibilityLabel(button.title)
                            .accessibilityIdentifier("touch-layout-\(button.rawValue)")
                            .onTapGesture {
                                selected = button
                                dragStart = nil
                            }
                            .gesture(DragGesture(minimumDistance: 1, coordinateSpace: .named("touch-layout-canvas")).onChanged { value in
                                if selected != button || dragStart == nil {
                                    selected = button; dragStart = CGPoint(x: r.midX, y: r.midY)
                                }
                                guard let start = dragStart else { return }
                                draft.move(button, to: CGPoint(x: start.x + value.translation.width / scale, y: start.y + value.translation.height / scale), in: gui)
                            }.onEnded { _ in dragStart = nil })
                            .simultaneousGesture(MagnificationGesture().updating($isPinching) { _, active, _ in
                                active = true
                            }.onChanged { value in
                                selected = button
                                let original = pinchStart ?? draft.relativeSize(button, in: gui)
                                pinchStart = original
                                draft.resize(button, factor: value, in: gui, from: original)
                            }.onEnded { _ in pinchStart = nil })
                    }

                ForEach($draft.customButtons) { $custom in
                    TouchLayoutCustomControl(button: $custom, canvas: gui, scale: scale) {
                        editingCustom = custom.id
                    }
                        .contextMenu {
                            Button("Edit bindings"){editingCustom=custom.id}
                            Button("Duplicate"){var copy=custom;copy.id=UUID();draft.customButtons.append(copy)}
                            Button("Delete",role:.destructive){let id=custom.id;draft.customButtons.removeAll{$0.id==id}}
                        }
                }
                VStack(spacing: 0) {
                    Image(systemName: "line.3.horizontal")
                        .frame(maxWidth: .infinity).frame(height: 28)
                        .contentShape(Rectangle())
                        .accessibilityLabel("Move editing toolbar")
                        .accessibilityIdentifier("touch-toolbar-handle")
                        .gesture(DragGesture(coordinateSpace: .named("touch-layout-canvas")).onChanged { value in
                            let origin = toolbarStart ?? toolbarCentre ?? CGPoint(x: width / 2, y: height / 2)
                            toolbarStart = origin
                            toolbarCentre = toolbar.fittedCentre(CGPoint(
                                x: origin.x + value.translation.width,
                                y: origin.y + value.translation.height))
                        }.onEnded { _ in toolbarStart = nil })
            HStack(spacing: toolbar.compact ? 0 : 20) {
                toolbarAction("Cancel", icon: "xmark", id: "touch-editor-cancel", compact: toolbar.compact) {
                    finish(nil)
                }
                if !toolbar.compact { Spacer(minLength: 4) }
                Menu {
                    Button("Add custom button") {var button=CustomTouchButton();button.centre=TouchCentre(x:0.5,y:0.75);draft.customButtons.append(button);editingCustom=button.id}
                    ForEach(draft.customButtons) { button in Button(button.name){editingCustom=button.id} }
                    ForEach(draft.buttons) { button in
                        Button(button.title) { selected = button; dragStart = nil }
                    }
                } label: {
                    if toolbar.compact {
                        Image(systemName: "hand.draw")
                            .frame(maxWidth: .infinity, minHeight: 44)
                            .contentShape(Rectangle())
                    } else {
                        Label(selected?.title ?? "Touch Layout", systemImage: "hand.draw")
                    }
                }
                .accessibilityLabel(selected?.title ?? "Touch Layout")
                .accessibilityIdentifier("touch-editor-select")
                if !toolbar.compact { Spacer(minLength: 4) }
                toolbarAction("Reset", icon: "arrow.counterclockwise", id: "touch-editor-reset", compact: toolbar.compact) {
                    confirmReset = true
                }
                toolbarAction("Save", icon: "checkmark", id: "touch-editor-save", compact: toolbar.compact) {
                    finish(draft)
                }.fontWeight(.semibold)
            }.padding(.horizontal, toolbar.compact ? 4 : 12).frame(height: 54)
                    if let selected {
                        let current = draft.relativeSize(selected, in: gui)
                        let range = current.proportionalWidthRange
                        HStack {
                            Text("Size")
                            Slider(value: Binding(get: {
                                min(range.upperBound, max(range.lowerBound, current.width))
                            }, set: { width in
                                // The slider can call its setter repeatedly before the view updates.
                                let latest = draft.relativeSize(selected, in: gui)
                                draft.resize(selected, factor: width / latest.width, in: gui)
                            }), in: range.lowerBound == range.upperBound ? 0.02...0.8 : range)
                                .disabled(range.lowerBound == range.upperBound)
                                .accessibilityIdentifier("touch-button-size")
                            Text("Opacity")
                            Slider(value:Binding(get:{ draft.opacity(selected) },set:{ draft.opacities[String(selected.rawValue)] = $0 }),in:0.1...1)
                        }.padding(.horizontal,16).padding(.bottom,12)
                    }

                }
                .frame(width: toolbar.width)
                .background(.ultraThinMaterial, in: RoundedRectangle(cornerRadius: 18))
                .position(toolbarCentre ?? CGPoint(x: width / 2, y: height / 2))
                .zIndex(2)
            }
            .frame(width: width, height: height)
            .coordinateSpace(name: "touch-layout-canvas")
            .onChange(of: isPinching) { active in
                if !active { pinchStart = nil }
            }
            .onChange(of: geometry.size) { _ in
                toolbarCentre = nil
                toolbarStart = nil
                dragStart = nil
                pinchStart = nil
            }
        }
        .ignoresSafeArea()
        .background(Color.black)
        .foregroundStyle(.white).preferredColorScheme(.dark)
        .sheet(isPresented:Binding(get:{editingCustom != nil},set:{if !$0 {editingCustom=nil}})) {
            NavigationStack {
                if let id=editingCustom,let index=draft.customButtons.firstIndex(where:{$0.id==id}) {
                    CustomTouchEditor(button:$draft.customButtons[index])
                        .toolbar {Button("Done"){editingCustom=nil}}
                }
            }
        }
        .confirmationDialog("Reset all button positions?", isPresented: $confirmReset, titleVisibility: .visible) {
            Button("Reset Layout", role: .destructive) {
                draft.resetLayout()
                selected = nil
                dragStart = nil
                pinchStart = nil
            }
        } message: { Text("This resets positions, sizes, and opacity. Custom bindings are kept. Save to keep the changes.") }
    }
    private func toolbarAction(_ title: String, icon: String, id: String, compact: Bool,
                               action: @escaping () -> Void) -> some View {
        Button(action: action) {
            if compact {
                Image(systemName: icon)
                    .frame(maxWidth: .infinity, minHeight: 44)
                    .contentShape(Rectangle())
            } else {
                Text(title).frame(minWidth: 50, minHeight: 44)
            }
        }
        .accessibilityLabel(title)
        .accessibilityIdentifier(id)
    }
}

struct TouchEditorToolbarGeometry {
    let canvas: CGSize

    var width: CGFloat { min(500, canvas.width - min(32, canvas.width / 10)) }
    var compact: Bool { width < 380 }

    func fittedCentre(_ point: CGPoint) -> CGPoint {
        let horizontal = width / 2
        let vertical = min(80, canvas.height / 2)
        return CGPoint(x: min(canvas.width - horizontal, max(horizontal, point.x)),
                       y: min(canvas.height - vertical, max(vertical, point.y)))
    }
}

private struct TouchLayoutCustomControl: View {
    @Binding var button: CustomTouchButton
    let canvas: CGSize
    let scale: CGFloat
    let edit: () -> Void
    @GestureState private var translation = CGSize.zero

    init(button: Binding<CustomTouchButton>, canvas: CGSize, scale: CGFloat, edit: @escaping () -> Void) {
        _button = button
        self.canvas = canvas
        self.scale = scale
        self.edit = edit
    }

    private func moved(_ offset: CGSize) -> CustomTouchButton {
        button.moved(by: CGSize(width: offset.width / scale, height: offset.height / scale), in: canvas)
    }

    var body: some View {
        let rect = moved(translation).rect(in: canvas)
        let shape = RoundedRectangle(cornerRadius: 6)
        Text(button.name)
            .foregroundStyle(.white)
            .font(.system(size: 17))
            .lineLimit(1)
            .minimumScaleFactor(0.4)
            .frame(width: rect.width * scale, height: rect.height * scale)
            .background(.black.opacity(0.4), in: shape)
            .overlay { shape.strokeBorder(.white.opacity(0.6)) }
            .opacity(button.effectiveOpacity)
            .contentShape(Rectangle())
            .position(x: rect.midX * scale, y: rect.midY * scale)
            .accessibilityIdentifier("touch-layout-custom-\(button.id)")
            .onTapGesture(perform: edit)
            .gesture(DragGesture(minimumDistance: 1, coordinateSpace: .named("touch-layout-canvas"))
                .updating($translation) { value, offset, _ in offset = value.translation }
                .onEnded { value in button = moved(value.translation) })
    }
}

/// Same integer scale selection and ceil-to-GUI dimensions as HudBatch::new.
struct TouchEditorGeometry {
    let gui: CGSize
    let pointsPerUnit: CGFloat
    init(size: CGSize, displayScale: CGFloat, limit: Int, forceUnicode: Bool = false) {
        let density = max(1, displayScale)
        let pixels = (Int((size.width * density).rounded()), Int((size.height * density).rounded()))
        var scale = 1
        while (limit == 0 || scale < limit) && pixels.0 / (scale + 1) >= 320 && pixels.1 / (scale + 1) >= 240 {
            scale += 1
        }
        if forceUnicode, scale % 2 != 0 { scale += 1 }
        gui = CGSize(width: ceil(Double(pixels.0) / Double(scale)), height: ceil(Double(pixels.1) / Double(scale)))
        pointsPerUnit = CGFloat(scale) / density
    }
    static func savedOptions(
        at url: URL = GameFolder.documents.appendingPathComponent("options.txt")
    ) -> (limit: Int, forceUnicode: Bool) {
        guard let text = try? String(contentsOf: url, encoding: .utf8) else { return (0, false) }
        let lines = text.split(whereSeparator: \.isNewline)
        func value(_ name: String) -> Substring? {
            let prefix = name + ":"
            return lines.last(where: { $0.hasPrefix(prefix) })?.dropFirst(prefix.count)
        }
        let limit = value("guiScale").flatMap { Int($0.trimmingCharacters(in: .whitespaces)) } ?? 0
        return (max(0, limit), value("forceUnicodeFont") == "true")
    }
}

/// UIKit recognizes the same tap/long-press/two-finger gestures used by the
/// reference launcher. The Rust frontend consumes their results on its frame
/// thread; the callbacks never access game state or player files.
final class TouchGestureBridge: NSObject, UIGestureRecognizerDelegate {
    static let shared = TouchGestureBridge()
    private weak var surface: UIView?
    private var recognizers: [UIGestureRecognizer] = []
    private var customViews: [UUID:CustomTouchView] = [:]
    private let customHolds=CustomTouchHolds()
    private var customPressed = Set<Int>()
    var customModifierBits: UInt32 {
        (customPressed.contains(340)||customPressed.contains(344) ? 1 : 0)
        | (customPressed.contains(341)||customPressed.contains(345) ? 2 : 0)
        | (customPressed.contains(342)||customPressed.contains(346) ? 4 : 0)
        | (customPressed.contains(343)||customPressed.contains(347) ? 8 : 0)
    }
    private var screen: UInt64 = 0
    private var active = false
    private var playing = false
    private var noWorld = false
    private var controlsHidden = false
    private var moreOpen = false
    private var closeShown = false
    private var keyboardShown = false
    private var hideCustomPaint = false
    private let configuration: () -> TouchConfiguration
    private let toggleMenuCursor: () -> Void
    private var gui = CGSize.zero
    private var guiScale: Double = 1
    private var pending: [(Double, [Double], UInt64, UInt32, UInt64)] = []
    private var needsEngineReset = false
    private var overflowed = false
    private var discardingInput = false
    var inputContext: UInt64 = 0
    private var hotbarHold: UILongPressGestureRecognizer?
    private var heldGestureSlot: Int?

    init(configuration: @escaping () -> TouchConfiguration = { TouchPreferences.shared.configuration },
         toggleMenuCursor: @escaping () -> Void = {
             let preferences = TouchPreferences.shared
             var config = preferences.configuration
             config.virtualMouse.toggle()
             preferences.save(config)
         }) {
        self.configuration = configuration
        self.toggleMenuCursor = toggleMenuCursor
        super.init()
    }

    func attach(to view: UIView) {
        guard surface !== view else { return }
        if !recognizers.isEmpty { suspend() }
        for button in customViews.values {
            button.isEnabled = false
            button.removeFromSuperview()
        }
        customViews.removeAll()
        if let surface { for recognizer in recognizers { surface.removeGestureRecognizer(recognizer) } }
        surface = view
        let tap = TouchSurfaceTap(target: self, action: #selector(tapped(_:)))
        let double = TouchSurfaceTap(target: self, action: #selector(doubleTapped(_:)))
        double.numberOfTapsRequired = 2
        let hold = TouchSurfaceHold(target: self, action: #selector(held(_:)))
        hold.minimumPressDuration = configuration().holdDuration
        hotbarHold = hold
        let keyboard = UILongPressGestureRecognizer(target: self, action: #selector(keyboardHeld(_:)))
        keyboard.numberOfTouchesRequired = 2
        let scroll = UIPanGestureRecognizer(target: self, action: #selector(scrolled(_:)))
        scroll.minimumNumberOfTouches = 2; scroll.maximumNumberOfTouches = 2
        let hotbar = TouchHotbarTracker(target: self, action: #selector(hotbarMoved(_:)))
        recognizers = [tap, double, hold, keyboard, scroll, hotbar]
        for recognizer in recognizers {
            recognizer.isEnabled = active
            recognizer.allowedTouchTypes = [NSNumber(value: UITouch.TouchType.direct.rawValue)]
            recognizer.cancelsTouchesInView = false
            recognizer.delegate = self
            view.addGestureRecognizer(recognizer)
        }
        scroll.cancelsTouchesInView = true
    }

    /// Launcher presentation can suspend engine frames before it polls the
    /// native controls again. Clear UIKit ownership now and reset the engine
    /// ledger ahead of the first new gesture when frames resume.
    func suspend() {
        active = false
        for button in customViews.values { button.isHidden = true }
        discardInput()
        needsEngineReset = true
    }

    private func discardInput() {
        discardingInput = true
        defer { discardingInput = false }
        // Clearing the event queue alone leaves UIKit tracking the old touch.
        // Disabling the recognizers cancels those in-flight gestures.
        setRecognizersEnabled(false)
        if active && !overflowed { setRecognizersEnabled(true) }
        for button in customViews.values { button.release(notify: false) }
        customHolds.reset()
        customPressed.removeAll()
        pending.removeAll()
        heldGestureSlot = nil
    }

    private func setRecognizersEnabled(_ enabled: Bool) {
        for recognizer in recognizers where recognizer.isEnabled != enabled {
            recognizer.isEnabled = enabled
        }
    }

    func poll(screen: UInt64, flags: UInt32, width: Double, height: Double, scale: Double, capacity: Int, withContext: Bool = false) -> [[Double]] {
        if flags & 4 != 0 {
            discardInput()
            return []
        }
        hotbarHold?.minimumPressDuration = configuration().holdDuration
        let enabled = flags & 2 == 0
        if !enabled, active {
            suspend()
        } else if self.screen != screen || active != enabled {
            discardInput()
        }
        if overflowed {
            // A control may have tracked another finger before the next poll.
            discardInput()
            overflowed = false
        }
        self.screen = screen; active = enabled; playing = flags & 1 != 0
        setRecognizersEnabled(enabled)
        noWorld = flags & 8 != 0
        controlsHidden = flags & 16 != 0; moreOpen = flags & 32 != 0
        closeShown = flags & 64 != 0; keyboardShown = flags & 128 != 0; hideCustomPaint = flags & 256 != 0
        gui = CGSize(width: width, height: height); guiScale = max(1, scale)
        updateCustomControls()
        guard enabled else { return [] }
        let now = ProcessInfo.processInfo.systemUptime
        if needsEngineReset {
            // Context zero makes this a reset even if the last rendered
            // menu's click context was evicted while the launcher was open.
            pending.insert((now, [0, 4, 0, 0, -1], 0, 0, 0), at: 0)
            needsEngineReset = false
        }
        let count = min(max(0, capacity), pending.count)
        let output = pending.prefix(count).map { timestamp, values, context, modifiers, touch in
            values + [max(0, now - timestamp)] + (withContext
                ? [Double(context & 0xffff_ffff), Double(context >> 32), Double(modifiers),
                   Double(touch & 0xffff_ffff), Double(touch >> 32)] : [])
        }
        pending.removeFirst(count)
        return output
    }

    private func updateCustomControls() {
        guard let surface else{return}
        let config=configuration(), valid=Set(config.customButtons.map(\.id))
        for id in Array(customViews.keys) where !valid.contains(id) {
            if let button = customViews.removeValue(forKey: id) {
                button.isEnabled = false
                button.release()
                button.removeFromSuperview()
            }
        }
        // Winit uses the render view's scale for its physical pixels. It can
        // differ from the display scale, including after a display change.
        let unit = guiScale / Double(surface.contentScaleFactor)
        var previousButton: CustomTouchView?
        for definition in config.customButtons {
            if let old = customViews[definition.id], old.definition != definition {
                old.isEnabled = false
                old.release()
                old.removeFromSuperview()
                customViews.removeValue(forKey: definition.id)
            }
            if customViews[definition.id] == nil {
                let button = CustomTouchView(definition, activate: { [weak self] flags in
                    guard let self, self.active, !self.overflowed else { return }
                    for code in definition.keys where TouchBinding.isActivationAction(code) {
                        if code == -5 {
                            if !self.playing { self.toggleMenuCursor() }
                        } else {
                            // The existing native protocol consumes these on the up phase.
                            self.emit(7, phase: 3, location: CGPoint(x: code, y: 0), modifiers: flags)
                            if self.overflowed { return }
                        }
                    }
                }) { [weak self] id,keys,down,flags in
                    guard let self, !self.overflowed else { return }
                    let heldKeys = keys.filter { !TouchBinding.isActivationAction($0) }
                    for (code,pressed) in self.customHolds.set(id,keys:heldKeys,down:down) {
                        if pressed {self.customPressed.insert(code)} else {self.customPressed.remove(code)}
                        self.emit(7,phase:pressed ? 1 : 3,location:CGPoint(x:code,y:0),modifiers:flags)
                        if self.overflowed { return }
                    }
                }
                customViews[definition.id] = button
                // The layout order determines which control wins an overlap.
                // Recreating an edited control must preserve its place.
                if let previousButton {
                    surface.insertSubview(button, aboveSubview: previousButton)
                } else if let first = surface.subviews.first(where: { $0 is CustomTouchView }) {
                    surface.insertSubview(button, belowSubview: first)
                } else {
                    surface.addSubview(button)
                }
            }
            guard let button=customViews[definition.id] else{continue}
            previousButton = button
            let hidden = !active || (controlsHidden && !definition.keys.contains(-2) && !definition.keys.contains(-5)) || (playing ? !definition.inGame : !definition.inMenu)
            if hidden {button.release()}
            button.isHidden=hidden
            let rect=definition.rect(in:gui)
            button.frame=CGRect(x:rect.minX*unit,y:rect.minY*unit,width:rect.width*unit,height:rect.height*unit)
            button.layer.opacity = config.showControls && !hideCustomPaint ? Float(definition.effectiveOpacity) : 0
        }
    }
    private func point(_ location: CGPoint) -> CGPoint {
        guard let surface, surface.bounds.width > 0, surface.bounds.height > 0 else { return .zero }
        let factor = Double(surface.contentScaleFactor) / guiScale
        return CGPoint(x: location.x * factor, y: location.y * factor)
    }
    private func slot(_ location: CGPoint) -> Int {
        guard playing, configuration().hotbarGestures else { return -1 }
        guard let surface else { return -1 }
        let displayScale = Double(surface.contentScaleFactor)
        return TouchHotbar.slot(point: CGPoint(x: location.x * displayScale, y: location.y * displayScale),
                                pixels: CGSize(width: (surface.bounds.width * displayScale).rounded(),
                                               height: (surface.bounds.height * displayScale).rounded()), scale: guiScale)
    }
    private func emit(_ kind: Double, phase: Int, location: CGPoint = .zero, slot: Int = -1,
                      modifiers: UIKeyModifierFlags = [], touch: UInt64 = 0) {
        guard !discardingInput, !overflowed, active || (kind == 7 && phase == 3) else { return }
        // If an event storm exhausts the bounded queue, cancel the old gesture
        // state rather than lose an up edge and leave a button held forever.
        if pending.count >= 128 {
            overflowed = true
            discardInput()
            needsEngineReset = true
            return
        }
        var virtualFlags:UIKeyModifierFlags=[]
        if customPressed.contains(340)||customPressed.contains(344){virtualFlags.insert(.shift)}
        if customPressed.contains(341)||customPressed.contains(345){virtualFlags.insert(.control)}
        if customPressed.contains(342)||customPressed.contains(346){virtualFlags.insert(.alternate)}
        if customPressed.contains(343)||customPressed.contains(347){virtualFlags.insert(.command)}
        let modifiers = modifiers.union(virtualFlags)
        let bits: UInt32 = (modifiers.contains(.shift) ? 1 : 0) | (modifiers.contains(.control) ? 2 : 0)
            | (modifiers.contains(.alternate) ? 4 : 0) | (modifiers.contains(.command) ? 8 : 0)
        pending.append((ProcessInfo.processInfo.systemUptime, [kind, Double(phase), location.x, location.y, Double(slot)], inputContext, bits, touch))
    }
    func gestureRecognizer(_ gestureRecognizer: UIGestureRecognizer, shouldReceive touch: UITouch) -> Bool {
        guard active, !discardingInput, !overflowed, gui.width > 0, gui.height > 0, let surface,
              surface.window?.rootViewController?.presentedViewController == nil else { return false }
        // Menu scrolling cancels UIKit touches. During play those same touches
        // own camera movement, so rejecting them in the handler is too late.
        if gestureRecognizer is UIPanGestureRecognizer && playing { return false }
        if touch.view is CustomTouchView {return false}
        let location = touch.location(in: surface), p = point(location)
        let configuration = configuration()
        let visible = configuration.buttons.filter { button in
            if noWorld { return false }
            if controlsHidden { return button == .hide }
            return moreOpen || ![TouchButton.perspective, .chat, .debug, .playerList].contains(button)
        }
        if visible.contains(where: { configuration.rect($0, in: gui).contains(p) }) { return false }
        let auxiliarySize = (min(48, max(26, gui.height * 0.14)) * 0.75).rounded()
        let closeRect = CGRect(x: gui.width - auxiliarySize - 4, y: 4, width: auxiliarySize, height: auxiliarySize)
        if closeShown && closeRect.contains(p) { return false }
        if keyboardShown && closeRect.offsetBy(dx: -auxiliarySize - 4, dy: 0).contains(p) { return false }
        let candidateSlot = slot(location)
        if gestureRecognizer is TouchHotbarTracker { return configuration.slideableHotbar && candidateSlot >= 0 }
        if let tap = gestureRecognizer as? TouchSurfaceTap, tap.numberOfTapsRequired == 2 {
            return candidateSlot >= 0
        }
        if gestureRecognizer is TouchSurfaceTap || gestureRecognizer is UILongPressGestureRecognizer {
            return noWorld || configuration.mouseGestures || candidateSlot >= 0 || (gestureRecognizer as? UILongPressGestureRecognizer)?.numberOfTouchesRequired == 2
        }
        return noWorld || configuration.mouseGestures
    }
    func gestureRecognizer(_ gestureRecognizer: UIGestureRecognizer,
                           shouldRecognizeSimultaneouslyWith otherGestureRecognizer: UIGestureRecognizer) -> Bool { true }
    @objc private func hotbarMoved(_ sender: TouchHotbarTracker) {
        guard configuration().slideableHotbar, playing, [.began, .changed].contains(sender.state) else { return }
        let next = slot(sender.location(in: surface))
        if next >= 0 && (sender.state == .began || next != sender.lastSlot) {
            emit(6, phase: sender.state.rawValue, slot: next, modifiers: sender.modifierFlags)
            sender.lastSlot = next
        }
    }
    @objc private func tapped(_ sender: TouchSurfaceTap) {
        guard sender.state == .recognized, sender.clickEligible else { return }
        let selectedSlot = sender.hotbarSlot(using: slot) ?? -1
        emit(1, phase: sender.state.rawValue, location: point(sender.location(in: surface)), slot: selectedSlot,
             modifiers: sender.modifierFlags, touch: sender.touchIdentity)
    }
    @objc private func doubleTapped(_ sender: TouchSurfaceTap) {
        guard sender.state == .recognized, sender.clickEligible,
              let first = sender.hotbarSlot(using: slot) else { return }
        emit(3, phase: sender.state.rawValue, slot: first, modifiers: sender.modifierFlags)
    }
    @objc private func held(_ sender: UILongPressGestureRecognizer) {
        guard let surface else { return }
        if sender.state == .began {
            guard active, !discardingInput, !overflowed else { return }
            if let touch = (sender as? TouchSurfaceHold)?.finger {
                for case let tap as TouchSurfaceTap in recognizers {
                    tap.excludeHeldTouch(touch)
                }
            }
            let slot = slot(sender.location(in: surface))
            heldGestureSlot = slot
            emit(2, phase: sender.state.rawValue, slot: slot, modifiers: sender.modifierFlags)
        } else if [.ended, .cancelled, .failed].contains(sender.state), let slot = heldGestureSlot {
            // A hold can move across the hotbar. Release the same action
            // that began, even if the finger is over a different target now.
            heldGestureSlot = nil
            emit(2, phase: sender.state.rawValue, slot: slot, modifiers: sender.modifierFlags)
        }
    }
    @objc private func keyboardHeld(_ sender: UILongPressGestureRecognizer) {
        if sender.state == .began { emit(5, phase: sender.state.rawValue, modifiers: sender.modifierFlags) }
    }
    @objc private func scrolled(_ sender: UIPanGestureRecognizer) {
        guard !playing, let surface, [.began, .changed, .ended].contains(sender.state) else { return }
        let velocity = sender.velocity(in: surface)
        guard surface.bounds.width > 0, surface.bounds.height > 0 else { return }
        emit(4, phase: sender.state.rawValue,
             location: CGPoint(x: velocity.x / surface.bounds.width, y: velocity.y / surface.bounds.height))
    }
}

/// Preserve the reference launcher's 5-by-5-point click eligibility rectangle
/// in addition to UIKit's own tap recognition. Moving out and back is a drag.
final class TouchSurfaceTap: UITapGestureRecognizer {
    private var clickRange = CGRect.zero
    private weak var finger: UITouch?
    private(set) var tapOrigins: [CGPoint] = []
    private(set) var clickEligible = true
    private(set) var touchIdentity: UInt64 = 0
    func hotbarSlot(using resolve: (CGPoint) -> Int) -> Int? {
        guard tapOrigins.count == numberOfTapsRequired, let start = tapOrigins.first else { return nil }
        let first = resolve(start)
        guard first >= 0, tapOrigins.dropFirst().allSatisfy({ resolve($0) == first }) else { return nil }
        return first
    }
    func trackTapStart(_ touch: UITouch) {
        guard finger == nil, tapOrigins.count < numberOfTapsRequired else { return }
        finger = touch
        touchIdentity = UInt64(UInt(bitPattern: Unmanaged.passUnretained(touch).toOpaque()))
        let p = touch.location(in: view)
        tapOrigins.append(p)
        clickRange = CGRect(x: p.x - 2, y: p.y - 2, width: 5, height: 5)
    }
    func excludeHeldTouch(_ touch: UITouch) {
        let held = UInt64(UInt(bitPattern: Unmanaged.passUnretained(touch).toOpaque()))
        if touchIdentity == held { clickEligible = false }
    }
    func trackTapEnd(_ touches: Set<UITouch>) {
        if let finger, touches.contains(finger) { self.finger = nil }
    }
    override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent) {
        if let touch = touches.first { trackTapStart(touch) }
        super.touchesBegan(touches, with: event)
    }
    override func touchesMoved(_ touches: Set<UITouch>, with event: UIEvent) {
        if let finger, touches.contains(finger), !clickRange.contains(finger.location(in: view)) {
            clickEligible = false
        }
        super.touchesMoved(touches, with: event)
    }
    override func touchesEnded(_ touches: Set<UITouch>, with event: UIEvent) {
        trackTapEnd(touches)
        super.touchesEnded(touches, with: event)
    }
    override func touchesCancelled(_ touches: Set<UITouch>, with event: UIEvent) {
        trackTapEnd(touches)
        super.touchesCancelled(touches, with: event)
    }
    override func reset() {
        finger = nil
        tapOrigins.removeAll()
        touchIdentity = 0
        clickEligible = true
        super.reset()
    }
}

/// A one-finger hold prevents that finger's tap without affecting other taps.
final class TouchSurfaceHold: UILongPressGestureRecognizer {
    private(set) weak var finger: UITouch?

    func trackHoldStart(_ touch: UITouch) {
        if finger == nil { finger = touch }
    }
    override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent) {
        if let touch = touches.first { trackHoldStart(touch) }
        super.touchesBegan(touches, with: event)
    }
    override func reset() {
        finger = nil
        super.reset()
    }
}

/// Six doubles per recognized input: kind, UIKit phase, x, y, hotbar slot,
/// and age in seconds. Called on UIKit's main thread by the game frame loop.
@_cdecl("horizon_touch_gestures_v1")
public func horizonTouchGestures(_ screen: UInt64, _ flags: UInt32, _ width: Double, _ height: Double,
                                 _ scale: Double, _ buffer: UnsafeMutablePointer<Double>?, _ capacity: UInt32) -> UInt32 {
    guard let buffer, capacity >= 6 else { return 0 }
    let events = TouchGestureBridge.shared.poll(screen: screen, flags: flags, width: width, height: height, scale: scale,
                                               capacity: Int(capacity / 6))
    for (index, values) in events.enumerated() {
        for component in 0..<6 { buffer[index * 6 + component] = values[component] }
    }
    return UInt32(events.count)
}

/// The reference bridge's physical-pixel hotbar selector (GUI scale applied
/// before integer centering), shared by native gesture hit tests.
enum TouchHotbar {
    static func slot(point: CGPoint, pixels: CGSize, scale: Double) -> Int {
        guard scale.isFinite, scale > 0, pixels.width.isFinite, pixels.height.isFinite,
              point.x.isFinite, point.y.isFinite, pixels.width >= 180 * scale,
              pixels.height >= 20 * scale else { return -1 }
        let width = Int(180 * scale), height = Int(20 * scale)
        let left = Int(pixels.width) / 2 - width / 2
        guard point.y >= Double(Int(pixels.height) - height), point.y < pixels.height,
              point.x >= Double(left), point.x < Double(left + width) else { return -1 }
        return min(8, Int((point.x - Double(left)) / Double(width) * 9))
    }
}

@_cdecl("horizon_touch_set_context_v1")
public func horizonTouchSetContext(_ context: UInt64) { TouchGestureBridge.shared.inputContext = context }

/// Version two appends context-id low/high words, hardware modifiers and touch-id
/// low/high words. Version one remains available for older bundled readers.
@_cdecl("horizon_touch_gestures_v2")
public func horizonTouchGesturesV2(_ screen: UInt64, _ flags: UInt32, _ width: Double, _ height: Double,
                                  _ scale: Double, _ buffer: UnsafeMutablePointer<Double>?, _ capacity: UInt32) -> UInt32 {
    guard let buffer, capacity >= 11 else { return 0 }
    let events = TouchGestureBridge.shared.poll(screen: screen, flags: flags, width: width, height: height,
                                               scale: scale, capacity: Int(capacity / 11), withContext: true)
    for (index, values) in events.enumerated() {
        for component in 0..<11 { buffer[index * 11 + component] = values[component] }
    }
    return UInt32(events.count)
}

/// A hotbar-only observer; it never cancels the game view's raw touch stream.
final class TouchHotbarTracker: UIGestureRecognizer {
    var lastSlot = -1
    private weak var finger: UITouch?
    override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent) {
        guard finger == nil, let first = touches.first else { return }
        finger = first; lastSlot = -1; state = .began
    }
    override func touchesMoved(_ touches: Set<UITouch>, with event: UIEvent) {
        if let finger, touches.contains(finger) { state = .changed }
    }
    override func touchesEnded(_ touches: Set<UITouch>, with event: UIEvent) {
        if let finger, touches.contains(finger) { state = .ended }
    }
    override func touchesCancelled(_ touches: Set<UITouch>, with event: UIEvent) {
        if let finger, touches.contains(finger) { state = .cancelled }
    }
    override func reset() { finger = nil; lastSlot = -1; super.reset() }
    override func location(in view: UIView?) -> CGPoint { finger?.location(in: view) ?? .zero }
}

@_cdecl("horizon_touch_modifiers_v1")
public func horizonTouchModifiers() -> UInt32 { TouchGestureBridge.shared.customModifierBits }
