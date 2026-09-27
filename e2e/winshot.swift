// 使い方: swift e2e/winshot.swift <pid> <out.png>
// 指定プロセスのウィンドウだけを撮影する（画面全体を撮ると他アプリの内容が写り込むため）。
import CoreGraphics
import Foundation
let pid = Int32(CommandLine.arguments[1])!
let out = CommandLine.arguments[2]
let list = CGWindowListCopyWindowInfo([.optionOnScreenOnly], kCGNullWindowID) as! [[String: Any]]
guard let w = list.first(where: { ($0[kCGWindowOwnerPID as String] as? Int32) == pid && ($0[kCGWindowLayer as String] as? Int) == 0 }),
      let id = w[kCGWindowNumber as String] as? Int else { print("no window"); exit(1) }
let p = Process()
p.executableURL = URL(fileURLWithPath: "/usr/sbin/screencapture")
p.arguments = ["-x", "-o", "-l", String(id), out]
try p.run(); p.waitUntilExit()
exit(p.terminationStatus)
