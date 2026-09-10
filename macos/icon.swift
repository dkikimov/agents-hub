// Renders AppIcon.iconset. Run via `make icon`; iconutil turns it into the .icns.
//
//   swift macos/icon.swift <out-dir>/AppIcon.iconset
//
// Every size is drawn at its own resolution rather than downscaled from 1024, so
// the 16pt chevron keeps a whole pixel of stroke instead of a grey smear.

import AppKit

let out = URL(fileURLWithPath: CommandLine.arguments.dropFirst().first ?? "AppIcon.iconset")
try? FileManager.default.createDirectory(at: out, withIntermediateDirectories: true)

func rgb(_ hex: UInt32) -> CGColor {
    CGColor(red: CGFloat((hex >> 16) & 0xFF) / 255, green: CGFloat((hex >> 8) & 0xFF) / 255,
            blue: CGFloat(hex & 0xFF) / 255, alpha: 1)
}

let backdropTop = rgb(0x3B4261)
let backdropBottom = rgb(0x1A1B26)
let chevron = rgb(0x7AA2F7)
let cursor = rgb(0x9ECE6A)

func draw(into ctx: CGContext, side: CGFloat) {
    let u = side / 1024
    let tile = CGRect(x: 100 * u, y: 110 * u, width: 824 * u, height: 824 * u)
    let squircle = CGPath(roundedRect: tile, cornerWidth: 185 * u, cornerHeight: 185 * u,
                          transform: nil)

    ctx.saveGState()
    ctx.setShadow(offset: CGSize(width: 0, height: -12 * u), blur: 32 * u,
                  color: CGColor(gray: 0, alpha: 0.35))
    ctx.addPath(squircle)
    ctx.setFillColor(backdropBottom)
    ctx.fillPath()
    ctx.restoreGState()

    ctx.saveGState()
    ctx.addPath(squircle)
    ctx.clip()
    let gradient = CGGradient(colorsSpace: CGColorSpaceCreateDeviceRGB(),
                              colors: [backdropTop, backdropBottom] as CFArray,
                              locations: [0, 1])!
    ctx.drawLinearGradient(gradient, start: CGPoint(x: tile.minX, y: tile.maxY),
                           end: CGPoint(x: tile.maxX, y: tile.minY), options: [])
    ctx.restoreGState()

    ctx.setLineWidth(74 * u)
    ctx.setLineCap(.round)
    ctx.setLineJoin(.round)
    ctx.setStrokeColor(chevron)
    ctx.move(to: CGPoint(x: 336 * u, y: 660 * u))
    ctx.addLine(to: CGPoint(x: 494 * u, y: 522 * u))
    ctx.addLine(to: CGPoint(x: 336 * u, y: 384 * u))
    ctx.strokePath()

    ctx.setFillColor(cursor)
    ctx.addPath(CGPath(roundedRect: CGRect(x: 560 * u, y: 350 * u, width: 190 * u, height: 68 * u),
                       cornerWidth: 34 * u, cornerHeight: 34 * u, transform: nil))
    ctx.fillPath()
}

for (name, side) in [("16x16", 16), ("16x16@2x", 32), ("32x32", 32), ("32x32@2x", 64),
                     ("128x128", 128), ("128x128@2x", 256), ("256x256", 256),
                     ("256x256@2x", 512), ("512x512", 512), ("512x512@2x", 1024)] {
    let ctx = CGContext(data: nil, width: side, height: side, bitsPerComponent: 8, bytesPerRow: 0,
                        space: CGColorSpaceCreateDeviceRGB(),
                        bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
    ctx.setAllowsAntialiasing(true)
    draw(into: ctx, side: CGFloat(side))

    let png = NSBitmapImageRep(cgImage: ctx.makeImage()!).representation(using: .png, properties: [:])!
    try png.write(to: out.appendingPathComponent("icon_\(name).png"))
}
