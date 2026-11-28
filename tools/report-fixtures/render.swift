// Renders synthetic checkup-report specs (fixtures/reports/**/*.json) into the
// files the tests import: a PDF with a text layer, a scanned (image-only) PDF
// and phone-style photos of each page. Fake data only; see regenerate.sh.
//
//   swiftc -O tools/report-fixtures/render.swift -o /tmp/render-reports
//   /tmp/render-reports fixtures/reports/acceptance/zh-checkup-2024.json ...

import CoreGraphics
import CoreImage
import CoreText
import Foundation
import ImageIO
import UniformTypeIdentifiers

struct Column: Decodable { let key: String; let label: String; let x: Double }
struct Row: Decodable {
    let name: String
    let abbr: String?
    let value: String
    let flag: String?
    let unit: String?
    let range: String?
}
struct Section: Decodable { let title: String; let rows: [Row] }
struct PageSpec: Decodable { let sections: [Section]; let notes: [String]? }
struct Photo: Decodable { let angle: Double; let blur: Double; let noise: Double; let perspective: Double? }
struct Spec: Decodable {
    let id: String
    let language: String
    let provider: String
    let title: String
    let header: [String]
    let columns: [Column]
    let pages: [PageSpec]
    let outputs: [String]
    let photo: Photo?
}

let pageW: CGFloat = 595, pageH: CGFloat = 842

func font(_ spec: Spec, size: CGFloat, bold: Bool = false) -> CTFont {
    let names = spec.language == "zh"
        ? (bold ? ["PingFangSC-Semibold", "STHeitiSC-Medium"] : ["PingFangSC-Regular", "STHeitiSC-Light"])
        : (bold ? ["Helvetica-Bold"] : ["Helvetica"])
    for n in names {
        let f = CTFontCreateWithName(n as CFString, size, nil)
        if (CTFontCopyPostScriptName(f) as String) == n { return f }
    }
    return CTFontCreateWithName(names[0] as CFString, size, nil)
}

/// Draws `text` with its top-left corner at (x, top) in top-left page coordinates.
func draw(_ ctx: CGContext, _ text: String, x: CGFloat, top: CGFloat, font f: CTFont, center: Bool = false) {
    let attrs: [NSAttributedString.Key: Any] = [
        NSAttributedString.Key(kCTFontAttributeName as String): f,
        NSAttributedString.Key(kCTForegroundColorAttributeName as String): CGColor(gray: 0.05, alpha: 1),
    ]
    let line = CTLineCreateWithAttributedString(NSAttributedString(string: text, attributes: attrs))
    let width = CTLineGetTypographicBounds(line, nil, nil, nil)
    let ascent = CTFontGetAscent(f)
    ctx.textPosition = CGPoint(x: center ? x - CGFloat(width) / 2 : x, y: pageH - top - ascent)
    CTLineDraw(line, ctx)
}

func cell(_ row: Row, _ key: String) -> String {
    switch key {
    case "name": return row.name
    case "abbr": return row.abbr ?? ""
    case "value": return row.value
    case "value_flag": return row.value + (row.flag ?? "")
    case "flag": return row.flag ?? ""
    case "unit": return row.unit ?? ""
    case "range": return row.range ?? ""
    default: return ""
    }
}

func drawPage(_ ctx: CGContext, _ spec: Spec, _ index: Int) {
    ctx.setFillColor(CGColor(gray: 1, alpha: 1))
    ctx.fill(CGRect(x: 0, y: 0, width: pageW, height: pageH))
    let zh = spec.language == "zh"
    var y: CGFloat = 40
    draw(ctx, spec.provider, x: pageW / 2, top: y, font: font(spec, size: 16, bold: true), center: true)
    y += 24
    draw(ctx, spec.title, x: pageW / 2, top: y, font: font(spec, size: 13, bold: true), center: true)
    y += 26
    for (i, h) in spec.header.enumerated() {
        draw(ctx, h, x: 50 + CGFloat(i % 2) * 260, top: y, font: font(spec, size: 9.5))
        if i % 2 == 1 { y += 15 }
    }
    if spec.header.count % 2 == 1 { y += 15 }
    y += 6
    ctx.setStrokeColor(CGColor(gray: 0.3, alpha: 1))
    ctx.setLineWidth(0.8)
    ctx.stroke(CGRect(x: 45, y: pageH - y, width: pageW - 90, height: 0.1))
    y += 10

    let page = spec.pages[index]
    for section in page.sections {
        draw(ctx, section.title, x: 50, top: y, font: font(spec, size: 11, bold: true))
        y += 18
        for c in spec.columns {
            draw(ctx, c.label, x: c.x, top: y, font: font(spec, size: 9.5, bold: true))
        }
        y += 15
        for row in section.rows {
            for c in spec.columns {
                let text = cell(row, c.key)
                if !text.isEmpty { draw(ctx, text, x: c.x, top: y, font: font(spec, size: 9.5)) }
            }
            y += 15.5
        }
        y += 10
    }
    for note in page.notes ?? [] {
        draw(ctx, note, x: 50, top: y, font: font(spec, size: 8.5))
        y += 13
    }
    let footer = zh ? "第 \(index + 1) 页 / 共 \(spec.pages.count) 页" : "Page \(index + 1) of \(spec.pages.count)"
    draw(ctx, footer, x: pageW / 2, top: pageH - 40, font: font(spec, size: 8.5), center: true)
}

func bitmap(_ spec: Spec, _ index: Int, scale: CGFloat) -> CGImage {
    let ctx = CGContext(data: nil, width: Int(pageW * scale), height: Int(pageH * scale), bitsPerComponent: 8,
                        bytesPerRow: 0, space: CGColorSpaceCreateDeviceRGB(),
                        bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue)!
    ctx.scaleBy(x: scale, y: scale)
    drawPage(ctx, spec, index)
    return ctx.makeImage()!
}

func writePDF(_ url: URL, pages: Int, _ body: (CGContext, Int) -> Void) {
    var box = CGRect(x: 0, y: 0, width: pageW, height: pageH)
    let info: [CFString: Any] = [kCGPDFContextCreator: "MirrorSelf synthetic fixture", kCGPDFContextTitle: url.lastPathComponent]
    let ctx = CGContext(url as CFURL, mediaBox: &box, info as CFDictionary)!
    for i in 0..<pages {
        ctx.beginPDFPage(nil)
        body(ctx, i)
        ctx.endPDFPage()
    }
    ctx.closePDF()
}

let ci = CIContext(options: [.useSoftwareRenderer: true])

/// Grain, blur, rotation and a desk background: a phone photo of a printout.
func photograph(_ page: CGImage, _ p: Photo, seed: Int) -> CGImage {
    var img = CIImage(cgImage: page)
    let extent = img.extent
    // Paper tint and uneven light.
    img = img.applyingFilter("CIColorMatrix", parameters: [
        "inputRVector": CIVector(x: 0.96, y: 0, z: 0, w: 0),
        "inputGVector": CIVector(x: 0, y: 0.94, z: 0, w: 0),
        "inputBVector": CIVector(x: 0, y: 0, z: 0.88, w: 0),
    ])
    let light = CIFilter(name: "CIRadialGradient", parameters: [
        "inputCenter": CIVector(x: extent.width * 0.35, y: extent.height * 0.7),
        "inputRadius0": extent.width * 0.2, "inputRadius1": extent.width * 1.1,
        "inputColor0": CIColor(red: 1, green: 1, blue: 1, alpha: 0),
        "inputColor1": CIColor(red: 0, green: 0, blue: 0, alpha: 0.22),
    ])!.outputImage!.cropped(to: extent)
    img = light.composited(over: img)
    if p.noise > 0 {
        let noise = CIFilter(name: "CIRandomGenerator")!.outputImage!
            .transformed(by: CGAffineTransform(translationX: CGFloat(seed * 37), y: CGFloat(seed * 11)))
            .cropped(to: extent)
            .applyingFilter("CIColorMatrix", parameters: [
                "inputRVector": CIVector(x: 1, y: 0, z: 0, w: 0), "inputGVector": CIVector(x: 1, y: 0, z: 0, w: 0),
                "inputBVector": CIVector(x: 1, y: 0, z: 0, w: 0),
                "inputAVector": CIVector(x: 0, y: 0, z: 0, w: CGFloat(p.noise)),
                "inputBiasVector": CIVector(x: -0.5, y: -0.5, z: -0.5, w: 0),
            ])
        img = noise.applyingFilter("CIAdditionCompositing", parameters: [kCIInputBackgroundImageKey: img]).cropped(to: extent)
    }
    if p.blur > 0 {
        img = img.clampedToExtent().applyingGaussianBlur(sigma: p.blur).cropped(to: extent)
    }
    if let k = p.perspective, k > 0 {
        let w = extent.width, h = extent.height
        img = img.applyingFilter("CIPerspectiveTransform", parameters: [
            "inputTopLeft": CIVector(x: w * k, y: h), "inputTopRight": CIVector(x: w * (1 - k * 0.4), y: h * (1 - k * 0.3)),
            "inputBottomLeft": CIVector(x: 0, y: 0), "inputBottomRight": CIVector(x: w, y: h * k * 0.2),
        ])
    }
    let angle = CGFloat(p.angle) * .pi / 180
    img = img.transformed(by: CGAffineTransform(rotationAngle: angle))
    let margin: CGFloat = 70
    let canvas = img.extent.insetBy(dx: -margin, dy: -margin)
    let desk = CIImage(color: CIColor(red: 0.36, green: 0.33, blue: 0.30)).cropped(to: canvas)
    img = img.composited(over: desk)
    return ci.createCGImage(img, from: canvas)!
}

func writeJPEG(_ url: URL, _ image: CGImage, quality: Double) {
    let dest = CGImageDestinationCreateWithURL(url as CFURL, UTType.jpeg.identifier as CFString, 1, nil)!
    CGImageDestinationAddImage(dest, image, [kCGImageDestinationLossyCompressionQuality: quality] as CFDictionary)
    CGImageDestinationFinalize(dest)
}

func grayscale(_ image: CGImage) -> CGImage {
    let ctx = CGContext(data: nil, width: image.width, height: image.height, bitsPerComponent: 8, bytesPerRow: 0,
                        space: CGColorSpaceCreateDeviceGray(), bitmapInfo: CGImageAlphaInfo.none.rawValue)!
    ctx.draw(image, in: CGRect(x: 0, y: 0, width: image.width, height: image.height))
    return ctx.makeImage()!
}

for path in CommandLine.arguments.dropFirst() {
    let url = URL(fileURLWithPath: path)
    let spec = try JSONDecoder().decode(Spec.self, from: Data(contentsOf: url))
    let dir = url.deletingLastPathComponent()
    let n = spec.pages.count
    for output in spec.outputs {
        switch output {
        case "pdf":
            writePDF(dir.appendingPathComponent("\(spec.id).pdf"), pages: n) { ctx, i in drawPage(ctx, spec, i) }
        case "scan":
            writePDF(dir.appendingPathComponent("\(spec.id).scan.pdf"), pages: n) { ctx, i in
                ctx.draw(grayscale(bitmap(spec, i, scale: 2.2)), in: CGRect(x: 0, y: 0, width: pageW, height: pageH))
            }
        case "photo":
            let p = spec.photo ?? Photo(angle: 1.2, blur: 0.7, noise: 0.06, perspective: nil)
            for i in 0..<n {
                let shot = photograph(bitmap(spec, i, scale: 2.0), p, seed: i + spec.id.count)
                writeJPEG(dir.appendingPathComponent("\(spec.id).photo-p\(i + 1).jpg"), shot, quality: 0.72)
            }
        default:
            FileHandle.standardError.write("unknown output \(output)\n".data(using: .utf8)!)
        }
    }
    print("rendered \(spec.id) (\(spec.outputs.joined(separator: ", ")))")
}
