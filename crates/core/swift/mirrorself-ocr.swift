// mirrorself-ocr: the macOS OCR helper behind the `apple-vision` model provider.
//
// Reads a report (PDF or image) on stdin and writes the recognized text as
// JSON on stdout, so decrypted bytes never touch the disk. PDF pages with a
// real text layer are read with PDFKit (exact text, confidence 1.0); scanned
// pages and photos go through Apple Vision text recognition.
//
//   mirrorself-ocr recognize --mime application/pdf [--lang zh|en] < report.pdf
//   mirrorself-ocr version
//
// Coordinates are [x0, y0, x1, y1] with the origin at the top left: PDF
// points for PDF pages, pixels for images. MirrorSelf compiles this file on
// first use (see crates/core/src/models/apple.rs); it is not a service.

import CoreGraphics
import Foundation
import ImageIO
import PDFKit
import Vision

let helperVersion = "1.1.0"

struct Word: Encodable {
    let text: String
    let bbox: [Double]
    let confidence: Double
}

struct Line: Encodable {
    let text: String
    let bbox: [Double]
    let confidence: Double
    let words: [Word]
}

struct Page: Encodable {
    let page: Int
    let width: Double
    let height: Double
    let source: String
    let lines: [Line]
    /// A second, English-only pass over the same image: MirrorSelf trusts a
    /// number, unit or range when both passes read it the same way.
    let alt_lines: [Line]?
}

struct Output: Encodable {
    let engine: String
    let engine_version: String
    let pages: [Page]
}

struct Failure: Error, CustomStringConvertible {
    let description: String
}

func round2(_ v: CGFloat) -> Double { (Double(v) * 100).rounded() / 100 }

func box(_ r: CGRect) -> [Double] { [round2(r.minX), round2(r.minY), round2(r.maxX), round2(r.maxY)] }

// MARK: Vision

/// The primary pass reads the report's language; the second pass is
/// English-only, which reads digits and units more reliably.
func passes(_ lang: String?) -> (primary: [String], primaryCorrection: Bool, alt: [String], altCorrection: Bool) {
    switch lang {
    case "en": return (["en-US"], true, ["en-US"], false)
    default: return (["zh-Hans", "en-US"], false, ["en-US"], true)
    }
}

/// Recognizes text in `image`; boxes are shifted by `offsetX` pixels and
/// divided by `scale` (pixels → output units).
func recognize(_ image: CGImage, languages: [String], correction: Bool, scale: CGFloat, offsetX: CGFloat = 0) throws -> [Line] {
    let request = VNRecognizeTextRequest()
    request.recognitionLevel = .accurate
    request.recognitionLanguages = languages
    request.usesLanguageCorrection = correction
    try VNImageRequestHandler(cgImage: image, options: [:]).perform([request])

    let w = CGFloat(image.width), h = CGFloat(image.height)
    func toTopLeft(_ n: CGRect) -> CGRect {
        CGRect(x: (n.minX * w + offsetX) / scale, y: (1 - n.maxY) * h / scale, width: n.width * w / scale, height: n.height * h / scale)
    }
    var lines: [Line] = []
    for observation in request.results ?? [] {
        guard let candidate = observation.topCandidates(1).first else { continue }
        let text = candidate.string
        let confidence = Double(candidate.confidence)
        var words: [Word] = []
        // Split on whitespace only: lab values, units and ranges keep their punctuation.
        var start: String.Index? = nil
        var i = text.startIndex
        func flush(_ end: String.Index) {
            guard let s = start else { return }
            let range = s..<end
            let rect = (try? candidate.boundingBox(for: range))?.map { toTopLeft($0.boundingBox) } ?? toTopLeft(observation.boundingBox)
            words.append(Word(text: String(text[range]), bbox: box(rect), confidence: confidence))
            start = nil
        }
        while i < text.endIndex {
            if text[i].isWhitespace { flush(i) } else if start == nil { start = i }
            i = text.index(after: i)
        }
        flush(text.endIndex)
        lines.append(Line(text: text, bbox: box(toTopLeft(observation.boundingBox)), confidence: confidence, words: words))
    }
    return lines
}

func overlaps(_ a: [Double], _ b: [Double]) -> Bool {
    let w = min(a[2], b[2]) - max(a[0], b[0]), h = min(a[3], b[3]) - max(a[1], b[1])
    guard w > 0, h > 0 else { return false }
    let smaller = min((a[2] - a[0]) * (a[3] - a[1]), (b[2] - b[0]) * (b[3] - b[1]))
    return w * h > smaller * 0.3
}

/// The whole image, then two overlapping vertical strips: on a full page
/// Vision sometimes drops a column of short numbers that it reads fine in a
/// narrower crop. Strip lines fill in what the full pass missed.
func recognizeTiled(_ image: CGImage, languages: [String], correction: Bool, scale: CGFloat) throws -> [Line] {
    var lines = try recognize(image, languages: languages, correction: correction, scale: scale)
    let w = CGFloat(image.width), h = CGFloat(image.height)
    for (a, b) in [(0.0, 0.58), (0.42, 1.0)] as [(CGFloat, CGFloat)] {
        let rect = CGRect(x: (w * a).rounded(), y: 0, width: (w * (b - a)).rounded(), height: h)
        guard let crop = image.cropping(to: rect) else { continue }
        for line in try recognize(crop, languages: languages, correction: correction, scale: scale, offsetX: rect.minX) {
            if !lines.contains(where: { overlaps($0.bbox, line.bbox) }) {
                lines.append(line)
            }
        }
    }
    return lines
}

func twoPasses(_ image: CGImage, lang: String?, scale: CGFloat) throws -> ([Line], [Line]) {
    let p = passes(lang)
    let lines = try recognizeTiled(image, languages: p.primary, correction: p.primaryCorrection, scale: scale)
    let alt = try recognizeTiled(image, languages: p.alt, correction: p.altCorrection, scale: scale)
    return (lines, alt)
}

// MARK: Images

func loadImage(_ data: Data) throws -> CGImage {
    guard let source = CGImageSourceCreateWithData(data as CFData, nil),
          let props = CGImageSourceCopyPropertiesAtIndex(source, 0, nil) as? [CFString: Any]
    else { throw Failure(description: "not a readable image") }
    let w = props[kCGImagePropertyPixelWidth] as? Int ?? 0
    let h = props[kCGImagePropertyPixelHeight] as? Int ?? 0
    // A thumbnail at full size applies the EXIF orientation of phone photos.
    let options: [CFString: Any] = [
        kCGImageSourceCreateThumbnailFromImageAlways: true,
        kCGImageSourceCreateThumbnailWithTransform: true,
        kCGImageSourceThumbnailMaxPixelSize: max(w, h, 1),
    ]
    guard let image = CGImageSourceCreateThumbnailAtIndex(source, 0, options as CFDictionary) else {
        throw Failure(description: "cannot decode the image")
    }
    return image
}

// MARK: PDF

/// Words and lines from a page's text layer. Word boxes come from
/// `selection(for:)`, which shares `page.string`'s indices
/// (`characterBounds(at:)` skips the line breaks PDFKit synthesizes).
func textLayerLines(_ page: PDFPage, height: CGFloat) -> [Line] {
    guard let string = page.string else { return [] }
    let ns = string as NSString
    var lines: [Line] = []
    var words: [Word] = []

    func flushLine() {
        guard !words.isEmpty else { return }
        let all = words.map { CGRect(x: $0.bbox[0], y: $0.bbox[1], width: $0.bbox[2] - $0.bbox[0], height: $0.bbox[3] - $0.bbox[1]) }
            .reduce(CGRect.null) { $0.union($1) }
        lines.append(Line(text: words.map(\.text).joined(separator: " "), bbox: box(all), confidence: 1.0, words: words))
        words = []
    }

    var start: Int? = nil
    func flushWord(_ end: Int) {
        guard let s = start else { return }
        start = nil
        let range = NSRange(location: s, length: end - s)
        guard let b = page.selection(for: range)?.bounds(for: page), !b.isEmpty else { return }
        let r = CGRect(x: b.minX, y: height - b.maxY, width: b.width, height: b.height)
        words.append(Word(text: ns.substring(with: range), bbox: box(r), confidence: 1.0))
    }

    let breaks = CharacterSet.whitespacesAndNewlines
    for i in 0..<ns.length {
        let unit = ns.character(at: i)
        let isBreak = UnicodeScalar(unit).map { breaks.contains($0) } ?? false
        if isBreak {
            flushWord(i)
            if unit == 0x0A || unit == 0x0D { flushLine() }
        } else if start == nil {
            start = i
        }
    }
    flushWord(ns.length)
    flushLine()
    return lines
}

func render(_ page: PDFPage, scale: CGFloat) throws -> CGImage {
    let bounds = page.bounds(for: .mediaBox)
    let w = Int(bounds.width * scale), h = Int(bounds.height * scale)
    guard let ctx = CGContext(data: nil, width: w, height: h, bitsPerComponent: 8, bytesPerRow: 0,
                              space: CGColorSpaceCreateDeviceRGB(),
                              bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue)
    else { throw Failure(description: "cannot allocate a page bitmap") }
    ctx.setFillColor(CGColor(red: 1, green: 1, blue: 1, alpha: 1))
    ctx.fill(CGRect(x: 0, y: 0, width: w, height: h))
    ctx.scaleBy(x: scale, y: scale)
    ctx.translateBy(x: -bounds.minX, y: -bounds.minY)
    page.draw(with: .mediaBox, to: ctx)
    guard let image = ctx.makeImage() else { throw Failure(description: "cannot render the page") }
    return image
}

func recognizePDF(_ data: Data, lang: String?) throws -> [Page] {
    guard let doc = PDFDocument(data: data) else { throw Failure(description: "not a readable PDF") }
    if doc.isLocked { throw Failure(description: "the PDF is password-protected") }
    var pages: [Page] = []
    for index in 0..<doc.pageCount {
        guard let page = doc.page(at: index) else { continue }
        let bounds = page.bounds(for: .mediaBox)
        let text = (page.string ?? "").filter { !$0.isWhitespace }
        if text.count >= 20 {
            pages.append(Page(page: index + 1, width: round2(bounds.width), height: round2(bounds.height),
                              source: "text_layer", lines: textLayerLines(page, height: bounds.height), alt_lines: nil))
        } else {
            let scale: CGFloat = 2.5  // ~180 dpi
            let image = try render(page, scale: scale)
            let (lines, alt) = try twoPasses(image, lang: lang, scale: scale)
            pages.append(Page(page: index + 1, width: round2(bounds.width), height: round2(bounds.height),
                              source: "ocr", lines: lines, alt_lines: alt))
        }
    }
    return pages
}

// MARK: Main

func osVersion() -> String {
    let v = ProcessInfo.processInfo.operatingSystemVersion
    return "\(v.majorVersion).\(v.minorVersion).\(v.patchVersion)"
}

func main() throws {
    var args = Array(CommandLine.arguments.dropFirst())
    guard let command = args.first else { throw Failure(description: "usage: mirrorself-ocr recognize --mime TYPE [--lang zh|en]") }
    args.removeFirst()
    let engineVersion = "helper \(helperVersion); macOS \(osVersion()); VNRecognizeTextRequest rev \(VNRecognizeTextRequest.currentRevision)"
    if command == "version" {
        print(engineVersion)
        return
    }
    guard command == "recognize" else { throw Failure(description: "unknown command '\(command)'") }
    var mime = "", lang: String? = nil
    while !args.isEmpty {
        let flag = args.removeFirst()
        let value = args.isEmpty ? "" : args.removeFirst()
        switch flag {
        case "--mime": mime = value
        case "--lang": lang = value.isEmpty ? nil : value
        default: throw Failure(description: "unknown option '\(flag)'")
        }
    }
    let data = FileHandle.standardInput.readDataToEndOfFile()
    let pages: [Page]
    if mime == "application/pdf" {
        pages = try recognizePDF(data, lang: lang)
    } else if mime.hasPrefix("image/") {
        let image = try loadImage(data)
        let (lines, alt) = try twoPasses(image, lang: lang, scale: 1)
        pages = [Page(page: 1, width: Double(image.width), height: Double(image.height), source: "ocr",
                      lines: lines, alt_lines: alt)]
    } else {
        throw Failure(description: "unsupported type '\(mime)'")
    }
    let encoder = JSONEncoder()
    encoder.outputFormatting = [.sortedKeys, .withoutEscapingSlashes]
    let out = try encoder.encode(Output(engine: "apple-vision", engine_version: engineVersion, pages: pages))
    FileHandle.standardOutput.write(out)
}

do {
    try main()
} catch {
    FileHandle.standardError.write("mirrorself-ocr: \(error)\n".data(using: .utf8)!)
    exit(1)
}
