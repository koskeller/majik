import AppKit
import SwiftUI

// Lay flat square artwork into Apple's app-icon shape: an 824 px continuous-corner rounded square
// centred on a transparent 1024 px canvas, with the drop shadow from Apple's icon template. This is
// what `actool` renders as the legacy fallback for an Icon Composer package, except that actool
// stops at 256 px; `script/generate-icons` runs this to get the 1024 px master every other icon
// format is scaled from.
//
//   swift script/lib/render-icon.swift <artwork.png> <output.png>
let arguments = CommandLine.arguments
guard arguments.count == 3 else {
    FileHandle.standardError.write(Data("usage: render-icon.swift <artwork.png> <output.png>\n".utf8))
    exit(2)
}
let source = URL(fileURLWithPath: arguments[1])
let destination = URL(fileURLWithPath: arguments[2])
guard let artwork = NSImage(contentsOf: source) else {
    FileHandle.standardError.write(Data("cannot read \(source.path)\n".utf8))
    exit(1)
}

let canvas: CGFloat = 1024
let icon: CGFloat = 824

struct IconView: View {
    let artwork: NSImage
    var body: some View {
        Image(nsImage: artwork)
            .resizable()
            .interpolation(.high)
            .frame(width: icon, height: icon)
            .clipShape(RoundedRectangle(cornerRadius: icon * 0.2237, style: .continuous))
            .shadow(color: .black.opacity(0.3), radius: 12, y: 10)
            .frame(width: canvas, height: canvas)
    }
}

@MainActor
func render() -> CGImage? {
    let renderer = ImageRenderer(content: IconView(artwork: artwork))
    renderer.scale = 1
    return renderer.cgImage
}
guard let image = MainActor.assumeIsolated({ render() }) else {
    FileHandle.standardError.write(Data("rendering failed\n".utf8))
    exit(1)
}
let bitmap = NSBitmapImageRep(cgImage: image)
guard let png = bitmap.representation(using: .png, properties: [:]) else {
    FileHandle.standardError.write(Data("PNG encoding failed\n".utf8))
    exit(1)
}
do {
    try png.write(to: destination)
} catch {
    FileHandle.standardError.write(Data("cannot write \(destination.path): \(error)\n".utf8))
    exit(1)
}
