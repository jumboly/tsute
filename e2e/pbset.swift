// 使い方: swift e2e/pbset.swift <uti> <file>
// 指定 UTI の生データとして Clipboard に載せる（アプリが動画を実データで載せるケースの再現用）
import AppKit
let args = CommandLine.arguments
let data = try Data(contentsOf: URL(fileURLWithPath: args[2]))
let pb = NSPasteboard.general
pb.clearContents()
exit(pb.setData(data, forType: NSPasteboard.PasteboardType(args[1])) ? 0 : 1)
