// removebg — strip the background from an image using the Vision framework's
// foreground instance mask (the same tech behind macOS's "Remove Background").
// Usage: removebg <input-image> <output.png>
// Exits 0 on success; non-zero (with a message on stderr) otherwise.

import Foundation
import Vision
import CoreImage
import AppKit

func fail(_ msg: String) -> Never {
    FileHandle.standardError.write((msg + "\n").data(using: .utf8)!)
    exit(1)
}

guard CommandLine.arguments.count >= 3 else {
    fail("usage: removebg <input> <output.png>")
}
let inPath = CommandLine.arguments[1]
let outPath = CommandLine.arguments[2]

guard let input = CIImage(contentsOf: URL(fileURLWithPath: inPath)) else {
    fail("cannot read input image")
}

guard #available(macOS 14.0, *) else {
    fail("Remove Background requires macOS 14 or later")
}

let request = VNGenerateForegroundInstanceMaskRequest()
let handler = VNImageRequestHandler(ciImage: input, options: [:])
do {
    try handler.perform([request])
    guard let result = request.results?.first else {
        fail("no foreground subject found")
    }
    let masked = try result.generateMaskedImage(
        ofInstances: result.allInstances,
        from: handler,
        croppedToInstancesExtent: false
    )
    let ciOut = CIImage(cvPixelBuffer: masked)
    let ctx = CIContext()
    guard let cg = ctx.createCGImage(ciOut, from: ciOut.extent) else {
        fail("could not render output")
    }
    let rep = NSBitmapImageRep(cgImage: cg)
    guard let data = rep.representation(using: .png, properties: [:]) else {
        fail("could not encode PNG")
    }
    try data.write(to: URL(fileURLWithPath: outPath))
    exit(0)
} catch {
    fail("vision error: \(error)")
}
