// 使い方: swift scripts/make-video-fixture.swift <out.mov> [seconds] [width] [height]
// 再現可能なテスト用動画（色が変化する合成フレーム, H.264）を生成する。
// 画面録画を使わないのは、fixture に個人の画面内容が写り込むのを避けるため。
import AVFoundation
import CoreVideo
import Foundation

let args = CommandLine.arguments
let out = URL(fileURLWithPath: args.count > 1 ? args[1] : "fixture.mov")
let seconds = args.count > 2 ? Int(args[2])! : 2
let width = args.count > 3 ? Int(args[3])! : 320
let height = args.count > 4 ? Int(args[4])! : 240
let fps: Int32 = 15
try? FileManager.default.removeItem(at: out)
let fileType: AVFileType = out.pathExtension.lowercased() == "mp4" ? .mp4 : .mov
let writer = try AVAssetWriter(outputURL: out, fileType: fileType)
let input = AVAssetWriterInput(mediaType: .video, outputSettings: [
    AVVideoCodecKey: AVVideoCodecType.h264, AVVideoWidthKey: width, AVVideoHeightKey: height,
])
let adaptor = AVAssetWriterInputPixelBufferAdaptor(assetWriterInput: input, sourcePixelBufferAttributes: [
    kCVPixelBufferPixelFormatTypeKey as String: kCVPixelFormatType_32BGRA,
    kCVPixelBufferWidthKey as String: width, kCVPixelBufferHeightKey as String: height,
])
writer.add(input)
writer.startWriting()
writer.startSession(atSourceTime: .zero)
let frames = seconds * Int(fps)
for i in 0..<frames {
    while !input.isReadyForMoreMediaData { usleep(1000) }
    var pb: CVPixelBuffer?
    CVPixelBufferPoolCreatePixelBuffer(nil, adaptor.pixelBufferPool!, &pb)
    CVPixelBufferLockBaseAddress(pb!, [])
    let base = CVPixelBufferGetBaseAddress(pb!)!.assumingMemoryBound(to: UInt8.self)
    let stride = CVPixelBufferGetBytesPerRow(pb!)
    for y in 0..<height {
        for x in 0..<width {
            let p = base + y * stride + x * 4
            p[0] = UInt8((x + i * 4) & 0xff); p[1] = UInt8((y + i * 2) & 0xff); p[2] = UInt8((i * 8) & 0xff); p[3] = 255
        }
    }
    CVPixelBufferUnlockBaseAddress(pb!, [])
    adaptor.append(pb!, withPresentationTime: CMTime(value: CMTimeValue(i), timescale: fps))
}
input.markAsFinished()
let sem = DispatchSemaphore(value: 0)
writer.finishWriting { sem.signal() }
sem.wait()
if writer.status != .completed { print("failed: \(String(describing: writer.error))"); exit(1) }
print(out.path)
