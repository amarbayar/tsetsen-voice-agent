// tsetsen-photos: the photo library and a picture viewer, for Tsetsen.
//
//   tsetsen-photos status
//   tsetsen-photos search --from <unix> --to <unix> [--limit N] [--screenshots]
//   tsetsen-photos export --max <pixels> --dir <dir> <id>...
//   tsetsen-photos show [--title <text>] <jpeg>...
//
// Prints one JSON value on stdout. Failures print {"error":{"code","message"}}
// and exit 1. `show` stays running until it is terminated. Nothing is ever
// written to the library: it is opened read-only, and exports are copies.

import AppKit
import Foundation
import ImageIO
import Photos
import UniformTypeIdentifiers

func emit(_ value: Any) -> Never {
    let data = try! JSONSerialization.data(withJSONObject: value, options: [.sortedKeys])
    FileHandle.standardOutput.write(data)
    FileHandle.standardOutput.write("\n".data(using: .utf8)!)
    exit(0)
}

func fail(_ code: String, _ message: String) -> Never {
    let data = try! JSONSerialization.data(
        withJSONObject: ["error": ["code": code, "message": message]], options: [.sortedKeys])
    FileHandle.standardOutput.write(data)
    FileHandle.standardOutput.write("\n".data(using: .utf8)!)
    exit(1)
}

func option(_ args: inout [String], _ name: String) -> String? {
    guard let i = args.firstIndex(of: name), i + 1 < args.count else { return nil }
    let value = args[i + 1]
    args.removeSubrange(i...(i + 1))
    return value
}

func stateName(_ status: PHAuthorizationStatus) -> String {
    switch status {
    case .authorized: return "authorized"
    case .limited: return "limited"
    case .denied: return "denied"
    case .restricted: return "restricted"
    case .notDetermined: return "not_determined"
    @unknown default: return "unknown"
    }
}

/// Asks once if never asked; fails unless access is granted.
func requireAccess() {
    var status = PHPhotoLibrary.authorizationStatus(for: .readWrite)
    if status == .notDetermined {
        let done = DispatchSemaphore(value: 0)
        PHPhotoLibrary.requestAuthorization(for: .readWrite) { s in
            status = s
            done.signal()
        }
        done.wait()
    }
    guard status == .authorized || status == .limited else {
        fail("PERMISSION_DENIED", "access to Photos is \(stateName(status))")
    }
}

func search(_ args: [String]) -> Never {
    var args = args
    guard let from = option(&args, "--from").flatMap(Double.init),
        let to = option(&args, "--to").flatMap(Double.init)
    else { fail("INVALID_ARGS", "search needs --from and --to, in Unix seconds") }
    let limit = max(0, option(&args, "--limit").flatMap(Int.init) ?? 500)
    // Screenshots are not what people mean by "our pictures".
    let screenshots = args.contains("--screenshots")
    requireAccess()
    let options = PHFetchOptions()
    options.predicate = NSPredicate(
        format: "mediaType == %d AND creationDate >= %@ AND creationDate < %@",
        PHAssetMediaType.image.rawValue,
        Date(timeIntervalSince1970: from) as NSDate,
        Date(timeIntervalSince1970: to) as NSDate)
    options.sortDescriptors = [NSSortDescriptor(key: "creationDate", ascending: false)]
    options.includeHiddenAssets = false
    let assets = PHAsset.fetchAssets(with: options)
    var out: [[String: Any]] = []
    assets.enumerateObjects { asset, _, stop in
        if out.count >= limit {
            stop.pointee = true
            return
        }
        if !screenshots && asset.mediaSubtypes.contains(.photoScreenshot) { return }
        var item: [String: Any] = [
            "id": asset.localIdentifier,
            "taken": asset.creationDate?.timeIntervalSince1970 ?? 0,
            "favorite": asset.isFavorite,
        ]
        if let location = asset.location {
            item["lat"] = location.coordinate.latitude
            item["lon"] = location.coordinate.longitude
        }
        out.append(item)
    }
    emit(out)
}

func writeJPEG(_ image: CGImage, to url: URL) -> Bool {
    guard
        let destination = CGImageDestinationCreateWithURL(
            url as CFURL, UTType.jpeg.identifier as CFString, 1, nil)
    else { return false }
    CGImageDestinationAddImage(
        destination, image, [kCGImageDestinationLossyCompressionQuality: 0.85] as CFDictionary)
    return CGImageDestinationFinalize(destination)
}

func export(_ args: [String]) -> Never {
    var args = args
    guard let maxPixels = option(&args, "--max").flatMap(Double.init), maxPixels > 0,
        let dir = option(&args, "--dir")
    else { fail("INVALID_ARGS", "export needs --max and --dir") }
    let ids = args.filter { $0 != "--" }
    requireAccess()
    let fetched = PHAsset.fetchAssets(withLocalIdentifiers: ids, options: nil)
    var byId: [String: PHAsset] = [:]
    fetched.enumerateObjects { asset, _, _ in byId[asset.localIdentifier] = asset }
    let manager = PHImageManager.default()
    let options = PHImageRequestOptions()
    options.isSynchronous = true
    options.deliveryMode = .highQualityFormat
    options.resizeMode = .exact
    options.isNetworkAccessAllowed = true  // pictures kept in iCloud
    var out: [[String: Any]] = []
    for (index, id) in ids.enumerated() {
        guard let asset = byId[id] else { fail("NOT_FOUND", "no photo \(id)") }
        let scale = min(1, maxPixels / Double(max(asset.pixelWidth, asset.pixelHeight, 1)))
        let size = CGSize(
            width: (Double(asset.pixelWidth) * scale).rounded(),
            height: (Double(asset.pixelHeight) * scale).rounded())
        var image: NSImage?
        manager.requestImage(for: asset, targetSize: size, contentMode: .aspectFit, options: options) {
            result, _ in image = result
        }
        guard let image, let cg = image.cgImage(forProposedRect: nil, context: nil, hints: nil)
        else { fail("EXPORT_FAILED", "could not read photo \(id)") }
        let url = URL(fileURLWithPath: dir).appendingPathComponent(String(format: "picture-%02d.jpg", index + 1))
        guard writeJPEG(cg, to: url) else { fail("EXPORT_FAILED", "could not write \(url.path)") }
        let bytes = (try? FileManager.default.attributesOfItem(atPath: url.path)[.size] as? NSNumber)?.intValue ?? 0
        out.append(["id": id, "path": url.path, "bytes": bytes])
    }
    emit(out)
}

/// Pictures in a grid, each with a large number, for people who cannot see
/// small print.
final class Sheet: NSView {
    let images: [NSImage]
    init(images: [NSImage]) {
        self.images = images
        super.init(frame: .zero)
    }
    required init?(coder: NSCoder) { nil }

    override func draw(_ dirty: NSRect) {
        NSColor(white: 0.1, alpha: 1).setFill()
        bounds.fill()
        let count = images.count
        guard count > 0 else { return }
        let columns = Int(ceil(sqrt(Double(count) * Double(bounds.width / max(bounds.height, 1)))))
            .clamped(1, count)
        let rows = Int(ceil(Double(count) / Double(columns)))
        let gap: CGFloat = 16
        let cellW = (bounds.width - gap * CGFloat(columns + 1)) / CGFloat(columns)
        let cellH = (bounds.height - gap * CGFloat(rows + 1)) / CGFloat(rows)
        for (i, image) in images.enumerated() {
            let column = i % columns
            let row = i / columns
            let cell = NSRect(
                x: gap + CGFloat(column) * (cellW + gap),
                y: bounds.height - gap - CGFloat(row + 1) * cellH - CGFloat(row) * gap,
                width: cellW, height: cellH)
            let size = image.size
            let fit = min(cell.width / max(size.width, 1), cell.height / max(size.height, 1))
            let drawn = NSRect(
                x: cell.midX - size.width * fit / 2, y: cell.midY - size.height * fit / 2,
                width: size.width * fit, height: size.height * fit)
            image.draw(in: drawn)
            let badge = min(max(min(cellW, cellH) * 0.18, 44), 96)
            let circle = NSRect(x: drawn.minX + 10, y: drawn.maxY - badge - 10, width: badge, height: badge)
            NSColor(white: 1, alpha: 0.92).setFill()
            NSBezierPath(ovalIn: circle).fill()
            let label = "\(i + 1)" as NSString
            let font = NSFont.systemFont(ofSize: badge * 0.6, weight: .bold)
            let attributes: [NSAttributedString.Key: Any] = [.font: font, .foregroundColor: NSColor.black]
            let textSize = label.size(withAttributes: attributes)
            label.draw(
                at: NSPoint(x: circle.midX - textSize.width / 2, y: circle.midY - textSize.height / 2),
                withAttributes: attributes)
        }
    }
}

extension Int {
    func clamped(_ low: Int, _ high: Int) -> Int { Swift.max(low, Swift.min(high, self)) }
}

func show(_ args: [String]) -> Never {
    var args = args
    let title = option(&args, "--title") ?? "Your pictures"
    let paths = args.filter { $0 != "--" }
    let images = paths.compactMap { NSImage(contentsOfFile: $0) }
    guard !images.isEmpty else { fail("INVALID_ARGS", "no pictures to show") }
    let app = NSApplication.shared
    app.setActivationPolicy(.accessory)
    let screen = NSScreen.main?.visibleFrame ?? NSRect(x: 0, y: 0, width: 1440, height: 900)
    let frame = screen.insetBy(dx: screen.width * 0.08, dy: screen.height * 0.08)
    let window = NSWindow(
        contentRect: frame, styleMask: [.titled, .closable, .resizable], backing: .buffered, defer: false)
    window.title = title
    window.contentView = Sheet(images: images)
    window.level = .floating
    window.isReleasedWhenClosed = false
    window.makeKeyAndOrderFront(nil)
    app.activate(ignoringOtherApps: true)
    signal(SIGTERM) { _ in exit(0) }
    // Closing the window ends the helper, so the owner sees it is gone.
    NotificationCenter.default.addObserver(
        forName: NSWindow.willCloseNotification, object: window, queue: .main
    ) { _ in exit(0) }
    print("{\"showing\":\(images.count)}")
    fflush(stdout)
    app.run()
    exit(0)
}

var args = Array(CommandLine.arguments.dropFirst())
guard !args.isEmpty else { fail("INVALID_ARGS", "usage: tsetsen-photos status|search|export|show") }
let command = args.removeFirst()
switch command {
case "status": emit(["photos": stateName(PHPhotoLibrary.authorizationStatus(for: .readWrite))])
case "search": search(args)
case "export": export(args)
case "show": show(args)
default: fail("INVALID_ARGS", "unknown command \(command)")
}
