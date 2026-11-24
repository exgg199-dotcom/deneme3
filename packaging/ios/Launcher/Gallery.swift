import Darwin
import ImageIO
import SwiftUI
import UIKit

/// A screenshot the game took (F2, or the touch screenshot button) into `Documents/screenshots`.
struct Shot: Identifiable, Hashable, Sendable {
    let url: URL
    let date: Date
    private let device: dev_t
    private let inode: ino_t
    private let size: off_t
    private let changed: Date
    var id: URL { url }

    /// Newest first, with a stable name order if two dates match.
    static func all(in directory: URL = GameFolder.screenshots) -> [Shot] {
        let files = (try? FileManager.default.contentsOfDirectory(
            at: directory, includingPropertiesForKeys: nil, options: [.skipsHiddenFiles]
        )) ?? []
        return files
            .filter { $0.pathExtension.lowercased() == "png" }
            .compactMap { try? snapshot($0) }
            .sorted { lhs, rhs in
                if lhs.date != rhs.date { return lhs.date > rhs.date }
                return lhs.url.lastPathComponent < rhs.url.lastPathComponent
            }
    }

    private static func snapshot(_ url: URL) throws -> Shot {
        var info = stat()
        try url.withUnsafeFileSystemRepresentation { path in
            guard let path else { throw POSIXError(.EINVAL) }
            guard lstat(path, &info) == 0 else {
                throw POSIXError(POSIXErrorCode(rawValue: errno) ?? .EIO)
            }
        }
        return try snapshot(url, info: info)
    }

    private static func snapshot(_ url: URL, info: stat) throws -> Shot {
        guard info.st_size > 0, info.st_mode & mode_t(S_IFMT) == mode_t(S_IFREG) else { throw POSIXError(.EINVAL) }
        func date(_ time: timespec) -> Date {
            Date(timeIntervalSince1970: Double(time.tv_sec) + Double(time.tv_nsec) / 1_000_000_000)
        }
        return Shot(url: url, date: date(info.st_mtimespec), device: info.st_dev, inode: info.st_ino,
                    size: info.st_size, changed: date(info.st_ctimespec))
    }

    private func verify(_ file: FileHandle) throws {
        var info = stat()
        guard fstat(file.fileDescriptor, &info) == 0 else {
            throw POSIXError(POSIXErrorCode(rawValue: errno) ?? .EIO)
        }
        guard try Self.snapshot(url, info: info) == self else { throw POSIXError(.EBUSY) }
    }

    private func openVerified() throws -> FileHandle {
        let descriptor = try url.withUnsafeFileSystemRepresentation { path in
            guard let path else { throw POSIXError(.EINVAL) }
            // Open one inode and refuse links or pipes that replaced the listed shot.
            let descriptor = open(path, O_RDONLY | O_NOFOLLOW | O_NONBLOCK | O_CLOEXEC)
            guard descriptor >= 0 else {
                throw POSIXError(POSIXErrorCode(rawValue: errno) ?? .EIO)
            }
            return descriptor
        }
        let input = FileHandle(fileDescriptor: descriptor, closeOnDealloc: true)
        try verify(input)
        return input
    }

    /// Decode from the listed file's descriptor. ImageIO can seek through the
    /// compressed bytes without reading or copying the entire PNG up front.
    func decode<T>(_ transform: (CGImageSource) -> T?) -> T? {
        guard size > 0, let file = try? openVerified() else { return nil }
        var callbacks = CGDataProviderDirectCallbacks(
            version: 0, getBytePointer: nil, releaseBytePointer: nil,
            getBytesAtPosition: { info, buffer, offset, length in
                guard let info else { return 0 }
                let file = Unmanaged<FileHandle>.fromOpaque(info).takeUnretainedValue()
                var count: Int
                repeat {
                    count = pread(file.fileDescriptor, buffer, length, offset)
                } while count < 0 && errno == EINTR
                return max(0, count)
            },
            releaseInfo: { info in
                if let info { Unmanaged<FileHandle>.fromOpaque(info).release() }
            }
        )
        let owner = Unmanaged.passRetained(file)
        guard let provider = CGDataProvider(directInfo: owner.toOpaque(), size: size, callbacks: &callbacks) else {
            owner.release()
            return nil
        }
        guard let source = CGImageSourceCreateWithDataProvider(
            provider, [kCGImageSourceShouldCache: false] as CFDictionary
        ), let result = transform(source), (try? verify(file)) != nil else {
            return nil
        }
        return result
    }

    func fullImage() -> UIImage? {
        decode { source in
            guard let metadata = CGImageSourceCopyPropertiesAtIndex(source, 0, nil) as? [CFString: Any],
                  let width = metadata[kCGImagePropertyPixelWidth] as? Int,
                  let height = metadata[kCGImagePropertyPixelHeight] as? Int else { return nil }
            // Bound the viewer's image memory.
            if max(width, height) > 4096 {
                let options = [
                    kCGImageSourceCreateThumbnailFromImageAlways: true,
                    kCGImageSourceCreateThumbnailWithTransform: true,
                    kCGImageSourceThumbnailMaxPixelSize: 4096,
                    kCGImageSourceShouldCacheImmediately: true,
                ] as CFDictionary
                return CGImageSourceCreateThumbnailAtIndex(source, 0, options).map { UIImage(cgImage: $0) }
            }
            guard let image = CGImageSourceCreateImageAtIndex(
                source, 0, [kCGImageSourceShouldCacheImmediately: true] as CFDictionary
            ) else { return nil }
            let raw = (metadata[kCGImagePropertyOrientation] as? NSNumber)?.uint32Value ?? 1
            let orientation: UIImage.Orientation
            switch CGImagePropertyOrientation(rawValue: raw) {
            case .upMirrored: orientation = .upMirrored
            case .down: orientation = .down
            case .downMirrored: orientation = .downMirrored
            case .leftMirrored: orientation = .leftMirrored
            case .right: orientation = .right
            case .rightMirrored: orientation = .rightMirrored
            case .left: orientation = .left
            default: orientation = .up
            }
            return UIImage(cgImage: image, scale: 1, orientation: orientation)
        }
    }

    func copyForSharing() throws -> SharedScreenshot {
        let input = try openVerified()
        defer { try? input.close() }

        let manager = FileManager.default
        let snapshot = try SharedScreenshot.stage(name: url.lastPathComponent)
        guard manager.createFile(atPath: snapshot.url.path, contents: nil) else {
            throw CocoaError(.fileWriteUnknown)
        }
        let output = try FileHandle(forWritingTo: snapshot.url)
        defer { try? output.close() }
        var remaining = size
        while remaining > 0 {
            try Task.checkCancellation()
            guard let bytes = try input.read(upToCount: Int(min(remaining, 1 << 20))), !bytes.isEmpty else {
                throw CocoaError(.fileReadCorruptFile)
            }
            try output.write(contentsOf: bytes)
            remaining -= off_t(bytes.count)
        }
        try verify(input)
        try output.close()
        return snapshot
    }

    func remove() throws {
        do {
            guard try Self.snapshot(url) == self else { throw POSIXError(.EBUSY) }
        } catch let error as POSIXError where error.code == .ENOENT {
            return
        }
        try url.withUnsafeFileSystemRepresentation { path in
            guard let path else { throw POSIXError(.EINVAL) }
            // Unlike FileManager.removeItem, unlink cannot recursively delete
            // a directory that replaced this file after the gallery listed it.
            if unlink(path) == 0 { return }
            let code = errno
            if code != ENOENT { throw NSError(domain: NSPOSIXErrorDomain, code: Int(code)) }
        }
    }
}

final class SharedScreenshot: Sendable {
    private static let stagingRoot: URL = {
        let manager = FileManager.default
        let root = manager.temporaryDirectory.appendingPathComponent("horizon-screenshot-shares", isDirectory: true)
        // Once per process, before any copy can be in use by a share sheet.
        try? manager.removeItem(at: root)
        return root
    }()

    fileprivate static func stage(name: String) throws -> SharedScreenshot {
        let directory = stagingRoot.appendingPathComponent(UUID().uuidString, isDirectory: true)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        return SharedScreenshot(directory: directory, name: name)
    }

    let url: URL

    private init(directory: URL, name: String) {
        url = directory.appendingPathComponent(name)
    }

    deinit {
        try? FileManager.default.removeItem(at: url.deletingLastPathComponent())
    }
}

@MainActor
private final class ScreenshotSharing: ObservableObject {
    @Published var snapshot: SharedScreenshot?
    @Published var failed = false
    @Published private(set) var preparing = false

    func prepare(_ shot: Shot) async {
        guard !preparing, snapshot == nil, !Task.isCancelled else { return }
        preparing = true
        defer { preparing = false }
        let worker = Task.detached(priority: .utility) { try shot.copyForSharing() }
        do {
            let copy = try await withTaskCancellationHandler {
                try await worker.value
            } onCancel: {
                worker.cancel()
            }
            if !Task.isCancelled { snapshot = copy }
        } catch {
            if !Task.isCancelled { failed = true }
        }
    }
}

private struct ScreenshotShareSheet: ViewModifier {
    @ObservedObject var sharing: ScreenshotSharing

    func body(content: Content) -> some View {
        content
            .background(ScreenshotActivity(sharing: sharing))
            .alert("Could not share this screenshot.", isPresented: $sharing.failed) {
                Button("OK", role: .cancel) {}
            }
    }
}

private struct ScreenshotActivity: UIViewControllerRepresentable {
    @ObservedObject var sharing: ScreenshotSharing

    func makeUIViewController(context: Context) -> Presenter { Presenter() }

    func updateUIViewController(_ controller: Presenter, context: Context) {
        controller.pending = sharing.snapshot
        controller.onFinish = { failed in
            sharing.snapshot = nil
            if failed { sharing.failed = true }
        }
        controller.presentIfReady()
    }

    final class Presenter: UIViewController {
        var pending: SharedScreenshot?
        var onFinish: ((Bool) -> Void)?
        private var active: SharedScreenshot?

        override func viewDidAppear(_ animated: Bool) {
            super.viewDidAppear(animated)
            presentIfReady()
        }

        func presentIfReady() {
            guard viewIfLoaded?.window != nil, presentedViewController == nil,
                  active == nil, let snapshot = pending else { return }
            active = snapshot
            pending = nil
            let controller = UIActivityViewController(activityItems: [snapshot.url], applicationActivities: nil)
            if traitCollection.userInterfaceIdiom == .pad {
                controller.modalPresentationStyle = .popover
                controller.popoverPresentationController?.sourceView = view
                controller.popoverPresentationController?.sourceRect = CGRect(
                    x: view.bounds.midX, y: view.bounds.midY, width: 1, height: 1
                )
                controller.popoverPresentationController?.permittedArrowDirections = []
            }
            controller.completionWithItemsHandler = { [weak self, snapshot] _, _, _, error in
                withExtendedLifetime(snapshot) {
                    self?.pending = nil
                    self?.active = nil
                    self?.onFinish?(error != nil)
                }
            }
            present(controller, animated: true)
        }
    }
}

/// Small pictures of the screenshots, made off the main thread and kept while the launcher is up.
@MainActor
final class Thumbnails: ObservableObject {
    @Published private(set) var images: [Shot: UIImage] = [:]
    private static let decoder = DispatchQueue(label: "Horizon screenshot thumbnails", qos: .utility)
    private let capacity: Int
    private var current: Set<Shot> = []
    private var visible: Set<Shot> = []
    private var asked: Set<Shot> = []
    private var recent: [Shot] = []

    init(capacity: Int = 64) {
        self.capacity = max(1, capacity)
    }

    func retain(_ shots: [Shot]) {
        current = Set(shots)
        visible.formIntersection(current)
        recent.removeAll { !current.contains($0) }
        asked.formIntersection(current)
        images = images.filter { current.contains($0.key) }
    }

    func show(_ shot: Shot, side: CGFloat) {
        guard current.contains(shot) else { return }
        visible.insert(shot)
        load(shot, side: side)
    }

    func hide(_ shot: Shot) {
        visible.remove(shot)
        trimCache()
    }

    func load(_ shot: Shot, side: CGFloat) {
        guard current.contains(shot) else { return }
        if images[shot] != nil {
            touch(shot)
            return
        }
        guard asked.insert(shot).inserted else { return }
        let longestSide = max(1, Int((side * 16 / 9 * 3).rounded(.up)))
        Self.decoder.async { [weak self] in
            guard self != nil else { return }
            let image = autoreleasepool { Self.decode(shot, longestSide: longestSide) }
            Task { @MainActor in self?.finish(shot, image: image) }
        }
    }

    nonisolated private static func decode(_ shot: Shot, longestSide: Int) -> UIImage? {
        let options = [
            kCGImageSourceCreateThumbnailFromImageAlways: true,
            kCGImageSourceCreateThumbnailWithTransform: true,
            kCGImageSourceThumbnailMaxPixelSize: longestSide,
        ] as CFDictionary
        return shot.decode { source in
            CGImageSourceCreateThumbnailAtIndex(source, 0, options).map { UIImage(cgImage: $0) }
        }
    }

    private func touch(_ shot: Shot) {
        recent.removeAll { $0 == shot }
        recent.append(shot)
    }

    private func finish(_ shot: Shot, image: UIImage?) {
        asked.remove(shot)
        guard current.contains(shot), let image else { return }
        touch(shot)
        images[shot] = image
        trimCache()
    }

    private func trimCache() {
        // A large viewport may need more images than the usual cache budget.
        while recent.count > capacity, let index = recent.firstIndex(where: { !visible.contains($0) }) {
            let oldest = recent.remove(at: index)
            images.removeValue(forKey: oldest)
        }
    }

}

/// Coalesce refreshes and publish only a scan that started after the latest request.
@MainActor
final class GalleryRefresh: ObservableObject {
    private let scan: @MainActor () async -> [Shot]
    private var revision = 0
    private var worker: Task<[Shot], Never>?

    init(scan: @escaping @MainActor () async -> [Shot] = {
        await Task.detached(priority: .utility) { Shot.all() }.value
    }) {
        self.scan = scan
    }

    func invalidate() {
        revision &+= 1
    }

    func reload(publish: ([Shot]) -> Void) async {
        guard !Task.isCancelled else { return }
        invalidate()
        let requested = revision
        if worker == nil {
            worker = Task {
                while true {
                    let started = revision
                    let shots = await scan()
                    if revision == started {
                        worker = nil
                        return shots
                    }
                }
            }
        }
        guard let worker else { return }
        let shots = await worker.value
        if requested == revision && !Task.isCancelled { publish(shots) }
    }
}

struct GalleryPage: View {
    @State private var shots: [Shot] = []
    @State private var opened: Shot?
    @State private var deleteFailed = false
    @StateObject private var thumbnails = Thumbnails()
    @StateObject private var refresh = GalleryRefresh()
    @StateObject private var sharing = ScreenshotSharing()
    @Environment(\.scenePhase) private var scenePhase
    private let columns = [GridItem(.adaptive(minimum: 150), spacing: 8)]

    var body: some View {
        LauncherScrollPage(refresh: { await reload() }) {
            galleryContents
        }
        .accessibilityIdentifier("gallery-scroll")
        .toolbar {
            ToolbarItem(placement: .primaryAction) {
                Button {
                    Task { await reload() }
                } label: {
                    Label("Refresh screenshots", systemImage: "arrow.clockwise")
                }
            }
        }
        .task { await reload() }
        .onChange(of: scenePhase) { phase in
            if phase == .active { Task { await reload() } }
        }
        .fullScreenCover(item: $opened) { shot in
            ShotViewer(shots: shots, start: shot, delete: delete)
        }
        .modifier(ScreenshotShareSheet(sharing: sharing))
        .alert("Could not delete this screenshot.", isPresented: $deleteFailed) {
            Button("OK", role: .cancel) {}
        }
    }

    private var galleryContents: some View {
        VStack(alignment: .leading, spacing: 16) {
            PageTitle(title: "Gallery", subtitle: shots.isEmpty ? nil : "\(shots.count) screenshot\(shots.count == 1 ? "" : "s")")
            if shots.isEmpty {
                EmptyNote(icon: "camera", text: "Screenshots you take in the game show up here, ready to share.")
            } else {
                LazyVGrid(columns: columns, spacing: 8) {
                    ForEach(shots) { shot in
                        Button {
                            Haptics.tap()
                            opened = shot
                        } label: {
                            thumbnail(shot)
                        }
                        .buttonStyle(PressStyle())
                        .accessibilityLabel("Screenshot \(shot.url.lastPathComponent)")
                        .accessibilityIdentifier("gallery-screenshot-\(shot.url.lastPathComponent)")
                        .contextMenu {
                            Button {
                                Task { await sharing.prepare(shot) }
                            } label: {
                                Label("Share", systemImage: "square.and.arrow.up")
                            }
                            .disabled(sharing.preparing)
                            Button(role: .destructive) {
                                deleteFailed = !delete(shot)
                            } label: { Label("Delete", systemImage: "trash") }
                        }
                    }
                }
            }
        }

    }

    private func thumbnail(_ shot: Shot) -> some View {
        Color.clear
            .aspectRatio(16 / 9, contentMode: .fit)
            .overlay {
                if let image = thumbnails.images[shot] {
                    Image(uiImage: image).resizable().scaledToFill()
                } else {
                    Theme.surfaceSunken
                }
            }
            .clipShape(RoundedRectangle(cornerRadius: 8))
            .overlay(RoundedRectangle(cornerRadius: 8).strokeBorder(Theme.border))
            .onAppear { thumbnails.show(shot, side: 110) }
            .onDisappear { thumbnails.hide(shot) }
            .id(shot)
    }

    private func reload() async {
        await refresh.reload { latest in
            thumbnails.retain(latest)
            shots = latest
        }
    }

    private func delete(_ shot: Shot) -> Bool {
        refresh.invalidate()
        defer { Task { await reload() } }
        do {
            try shot.remove()
            shots.removeAll { $0 == shot }
            thumbnails.retain(shots)
            return true
        } catch {
            return false
        }
    }
}

/// One screenshot at a time, swiped through, with Share and Delete.
private struct ShotViewer: View {
    let shots: [Shot]
    let delete: (Shot) -> Bool
    @State private var current: Shot
    @State private var confirming = false
    @State private var pendingDeletion: Shot?
    @State private var deleteFailed = false
    @StateObject private var sharing = ScreenshotSharing()
    @Environment(\.dismiss) private var dismiss

    init(shots: [Shot], start: Shot, delete: @escaping (Shot) -> Bool) {
        self.shots = shots
        self.delete = delete
        _current = State(initialValue: start)
    }

    var body: some View {
        ZStack {
            Color.black.ignoresSafeArea()
            TabView(selection: $current) {
                ForEach(shots) { shot in
                    FullImage(shot: shot).tag(shot)
                }
            }
            .tabViewStyle(.page(indexDisplayMode: .never))
            .ignoresSafeArea()
        }
        .overlay(alignment: .top) {
            HStack {
                Button { dismiss() } label: {
                    Image(systemName: "xmark").font(.system(size: 16, weight: .bold)).frame(width: 44, height: 44)
                }
                .accessibilityLabel("Close screenshot")
                Spacer()
                Text(current.date.formatted(date: .abbreviated, time: .shortened))
                    .font(Theme.mono(11))
                Spacer()
                Button {
                    let shot = current
                    Task { await sharing.prepare(shot) }
                } label: {
                    Image(systemName: "square.and.arrow.up").font(.system(size: 16, weight: .semibold))
                        .frame(width: 44, height: 44)
                }
                .disabled(sharing.preparing)
                .accessibilityLabel("Share screenshot")
                Button {
                    pendingDeletion = current
                    confirming = true
                } label: {
                    Image(systemName: "trash").font(.system(size: 16, weight: .semibold)).frame(width: 44, height: 44)
                }
            }
            .foregroundStyle(Theme.text)
            .padding(.horizontal, 8)
            .background(.black.opacity(0.45))
        }
        .modifier(ScreenshotShareSheet(sharing: sharing))
        .onChange(of: shots) { latest in
            if let next = latest.first(where: { $0.id == current.id }) ?? latest.first {
                current = next
            } else {
                dismiss()
            }
        }
        .confirmationDialog("Delete this screenshot?", isPresented: $confirming,
                            titleVisibility: .visible, presenting: pendingDeletion) { shot in
            Button("Delete", role: .destructive) {
                if delete(shot) { dismiss() }
                else { deleteFailed = true }
            }
        }
        .alert("Could not delete this screenshot.", isPresented: $deleteFailed) {
            Button("OK", role: .cancel) {}
        }

    }
}

private struct FullImage: View {
    private enum LoadState {
        case opening
        case loaded(UIImage)
        case unavailable
    }

    private struct LoadRequest: Hashable {
        let shot: Shot
        let retry: Int
    }

    let shot: Shot
    @State private var state: LoadState = .opening
    @State private var retry = 0

    var body: some View {
        Group {
            switch state {
            case .opening:
                ProgressView().tint(.white)
            case let .loaded(image):
                Image(uiImage: image).resizable().scaledToFit()
            case .unavailable:
                VStack(spacing: 14) {
                    Image(systemName: "photo.badge.exclamationmark").font(.title)
                    Text("Couldn’t open this screenshot.")
                    Button("Try again") { retry += 1 }
                        .buttonStyle(.bordered)
                }
                .foregroundStyle(.white)
                .padding(24)
            }
        }
        .task(id: LoadRequest(shot: shot, retry: retry)) {
            state = .opening
            let image = await Task.detached(priority: .userInitiated) {
                shot.fullImage()
            }.value
            if !Task.isCancelled { state = image.map(LoadState.loaded) ?? .unavailable }
        }
    }
}
