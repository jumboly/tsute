#!/usr/bin/env python3
"""同一 Mac 上の 2 Endpoint による E2E（Phase 1 完了条件の自動検証）。

使い方:
  python3 e2e/run_e2e.py --target local                 # ローカル開発サーバーを起動して実行
  python3 e2e/run_e2e.py --target cloud --env test      # デプロイ済み AWS 環境（infra/.build/test.json）で実行
オプション:
  --binary PATH   アプリ実行ファイル（既定: target/debug/tsute。.app 内の Contents/MacOS/tsute も可）
  --big-mb N      再開テストに使うファイルサイズ（既定 64MB）

注意:
- 実際の OS Clipboard を使う。実行前のテキストを退避し、終了時に復元する。
- 資格情報はテスト用の --insecure-file-credentials（Keychain ダイアログで無人実行が止まるのを避けるため。ADR-0011）。
- OS のドラッグ操作そのものは自動化できないため、Drop イベント以降（確認画面 → 送信）を UI 上で実行する（ADR-0013）。
"""
import argparse
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.request
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
from driver import App  # noqa: E402

ROOT = Path(__file__).resolve().parent.parent
FIX = ROOT / "fixtures"
results = []


def step(name):
    def deco(fn):
        def run(*a, **kw):
            t0 = time.time()
            print(f"--- {name}", flush=True)
            try:
                detail = fn(*a, **kw)
                results.append({"step": name, "ok": True, "secs": round(time.time() - t0, 1), "detail": detail})
                print(f"    OK ({time.time() - t0:.1f}s) {detail or ''}", flush=True)
            except Exception as e:
                results.append({"step": name, "ok": False, "secs": round(time.time() - t0, 1), "error": str(e)})
                print(f"    FAIL: {e}", flush=True)
                raise
        return run
    return deco


def sha(p):
    h = hashlib.sha256()
    with open(p, "rb") as f:
        for b in iter(lambda: f.read(1 << 20), b""):
            h.update(b)
    return h.hexdigest()


def pbcopy(text):
    subprocess.run(["pbcopy"], input=text.encode(), check=True)


def pbpaste():
    return subprocess.run(["pbpaste"], capture_output=True, check=True).stdout.decode()


def osascript(s):
    return subprocess.run(["osascript", "-e", s], capture_output=True, text=True, check=True).stdout.strip()


class Local:
    def __init__(self, work):
        self.dir = work / "server"
        bin_ = ROOT / "target/debug/tsute-devserver"
        subprocess.run(["cargo", "build", "-q", "-p", "tsute-server-local"], cwd=ROOT, check=True)
        self.proc = subprocess.Popen([str(bin_), "--bind", "127.0.0.1:0", "--data-dir", str(self.dir), "--blob-delay-ms", "1500"],
                                     stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        self.base_url = None
        for line in self.proc.stdout:
            if line.startswith("TSUTE_DEVSERVER_URL="):
                self.base_url = line.strip().split("=", 1)[1]
                break
        if not self.base_url:
            raise RuntimeError("devserver did not start")
        self.token = (self.dir / "admin-token").read_text()

    def issue_key(self):
        req = urllib.request.Request(f"{self.base_url}/admin/enrollment-keys", method="POST", headers={"x-admin-token": self.token})
        return json.load(urllib.request.urlopen(req))["enrollment_key"]

    def stop(self):
        self.proc.terminate()


class Cloud:
    def __init__(self, env):
        self.env = env
        out = json.loads((ROOT / f"infra/.build/{env}.json").read_text())
        self.base_url = out["app_base_url"]

    def issue_key(self):
        r = subprocess.run([str(ROOT / "scripts/admin.sh"), self.env, "issue-key"], capture_output=True, text=True, check=True)
        return json.loads(r.stdout)["enrollment_key"]

    def stop(self):
        pass


def launch(binary, profile, work, extra=()):
    dl = work / f"dl-{profile}"
    return App(binary, profile, work / "app", ["--insecure-file-credentials", "--download-dir", str(dl), *extra]).connect()


def incoming_done(app, kind, exclude=()):
    ids = app.js(f"return [...document.querySelectorAll('#history li[data-direction=incoming][data-kind={kind}][data-status=done]')].map(l => l.dataset.id)")
    new = [i for i in ids if i not in exclude]
    return new[0] if new else None


def item_click(app, tid, testid):
    app.js(f"""const b = document.querySelector('#history li[data-id="{tid}"] [data-testid="{testid}"]');
               if (!b) throw new Error('no {testid} button'); b.click(); return 1;""")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--target", choices=["local", "cloud"], default="local")
    ap.add_argument("--env", default="test")
    ap.add_argument("--binary", default=str(ROOT / "target/debug/tsute"))
    ap.add_argument("--big-mb", type=int, default=64)
    ap.add_argument("--keep", action="store_true", help="作業ディレクトリを残す")
    args = ap.parse_args()

    work = Path(tempfile.mkdtemp(prefix="tsute-e2e-", dir=os.environ.get("TMPDIR")))
    (work / "app").mkdir()
    saved_clip = pbpaste()
    server = Local(work) if args.target == "local" else Cloud(args.env)
    print(f"target={args.target} base_url={server.base_url} work={work}")
    apps = {}
    try:
        run_all(args, server, work, apps)
    finally:
        for a in apps.values():
            a.quit()
        server.stop()
        pbcopy(saved_clip)
        out = ROOT / "e2e/out"
        out.mkdir(exist_ok=True)
        report = {"target": args.target, "base_url": server.base_url, "binary": args.binary,
                  "time": time.strftime("%Y-%m-%dT%H:%M:%S%z"), "results": results}
        (out / f"e2e-{args.target}.json").write_text(json.dumps(report, ensure_ascii=False, indent=2))
        if not args.keep:
            shutil.rmtree(work, ignore_errors=True)
    ok = all(r["ok"] for r in results)
    print(f"\n{'PASS' if ok else 'FAIL'}: {sum(r['ok'] for r in results)}/{len(results)} steps")
    sys.exit(0 if ok else 1)


def run_all(args, server, work, apps):
    binary = args.binary

    @step("launch two endpoints (test-a, test-b) and enroll via UI")
    def enroll():
        for prof, name in (("test-a", "E2E Mac A"), ("test-b", "E2E Mac B")):
            a = launch(binary, prof, work)
            apps[prof] = a
            a.wait(lambda: a.cmd(cmd="window_open"), "enroll window shown for unenrolled profile")
            a.wait(lambda: a.js("return document.body.dataset.ready === '1'"), "ready")
            assert a.view() == "enroll"
            a.fill("enroll-url", server.base_url)
            a.fill("enroll-key", server.issue_key())
            a.fill("enroll-name", name)
            a.click("enroll-submit")
            a.wait(lambda: a.view() == "main", "main view", timeout=30)
            a.wait(lambda: a.js("return document.body.dataset.connection") == "online", "websocket online", timeout=30)
        return "enrolled"
    enroll()
    A, B = apps["test-a"], apps["test-b"]

    @step("enrollment key is single use")
    def single_use():
        key = server.issue_key()
        C = launch(binary, "test-c", work)
        try:
            C.wait(lambda: C.js("return document.body.dataset.ready === '1'"), "ready")
            for attempt in range(2):
                C.fill("enroll-url", server.base_url)
                C.fill("enroll-key", key)
                C.fill("enroll-name", "E2E C")
                C.click("enroll-submit")
                if attempt == 0:
                    C.wait(lambda: C.view() == "main", "first enrollment succeeds")
                    C.js("return (async () => { await window.__TAURI__.core.invoke('forget_enrollment'); location.reload(); })()")
                    C.wait(lambda: C.js("return document.body.dataset.ready === '1' && document.body.dataset.view === 'enroll'"), "enroll view again")
                else:
                    C.wait(lambda: "invalid" in (C.text("enroll-error") or "").lower() or "403" in (C.text("enroll-error") or ""), "reuse rejected")
            return C.text("enroll-error")
        finally:
            C.quit()
    single_use()

    @step("endpoint list shows peer online")
    def endpoints():
        A.wait(lambda: A.js("return [...document.querySelectorAll('#target option')].some(o => o.textContent.includes('E2E Mac B'))"), "B in A's targets")
        A.js("const s = document.getElementById('target'); s.value = [...s.options].find(o => o.textContent.includes('E2E Mac B')).value; return 1")
        return A.js("return [...document.querySelectorAll('#target option')].map(o => o.textContent).join(', ')")
    endpoints()

    seen = set()

    def recv(kind, timeout=120):
        tid = B.wait(lambda: incoming_done(B, kind, seen), f"B receives {kind}", timeout=timeout)
        seen.add(tid)
        return tid

    @step("Clipboard Text: preview → send → receive → explicit apply")
    def text():
        msg = f"つて E2E テキスト 🌏 {time.time()}\n2行目"
        pbcopy(msg)
        assert A.view() == "main"
        A.click("send-clipboard")
        A.wait(lambda: A.view() == "clip", "preview shown")
        assert A.text("clip-text") == msg, "preview must show exact clipboard text"
        # プレビュー表示だけでは送信されない
        time.sleep(1.5)
        assert incoming_done(B, "clipboard_text", seen) is None, "must not send before pressing Send"
        A.click("clip-send")
        tid = recv("clipboard_text")
        pbcopy("sentinel")
        time.sleep(0.5)
        assert pbpaste() == "sentinel", "receiving must not overwrite OS clipboard"
        item_click(B, tid, "apply")
        B.wait(lambda: pbpaste() == msg, "clipboard updated after apply")
        return f"{len(msg)} chars"
    text()

    @step("Clipboard Text large (>64KiB, object storage path)")
    def big_text():
        msg = ("大きいテキスト " * 12000)[:70000]
        pbcopy(msg)
        A.click("send-clipboard")
        A.wait(lambda: A.view() == "clip", "preview")
        A.click("clip-send")
        tid = recv("clipboard_text")
        item_click(B, tid, "apply")
        B.wait(lambda: pbpaste() == msg, "big text applied")
        return f"{len(msg.encode())} bytes"
    big_text()

    @step("Clipboard Image (PNG): preview with dimensions → receive → apply as image")
    def image():
        osascript(f'set the clipboard to (read (POSIX file "{FIX}/image-64x48.png") as «class PNGf»)')
        A.click("send-clipboard")
        A.wait(lambda: A.view() == "clip", "preview")
        meta = A.text("clip-meta")
        assert "64 × 48" in meta and A.js("return !!document.querySelector('[data-testid=clip-image]')"), meta
        A.click("clip-send")
        tid = recv("clipboard_image")
        pbcopy("sentinel")
        item_click(B, tid, "apply")
        B.wait(lambda: "PNGf" in osascript("clipboard info"), "PNG on clipboard")
        return meta
    image()

    @step("Clipboard Video (file URL, Finder-style copy): metadata → receive → apply as file URL")
    def video_url():
        mov = FIX / "video-320x240-2s.mov"
        osascript(f'set the clipboard to (POSIX file "{mov}")')
        A.click("send-clipboard")
        A.wait(lambda: A.view() == "clip", "preview")
        meta = A.text("clip-meta")
        assert "320 × 240" in meta and "2秒" in meta, meta
        A.click("clip-send")
        tid = recv("clipboard_video")
        item_click(B, tid, "apply")
        path = B.wait(lambda: osascript("POSIX path of (the clipboard as «class furl»)"), "file URL on clipboard")
        assert sha(path) == sha(mov), "received video differs"
        return meta
    video_url()

    @step("Clipboard Video (raw movie data on pasteboard)")
    def video_raw():
        mov = FIX / "video-320x240-2s.mov"
        # AppleScript の «class moov» は QuickTime の UTI にならないため、NSPasteboard に直接載せる
        subprocess.run(["swift", str(ROOT / "e2e/pbset.swift"), "com.apple.quicktime-movie", str(mov)], check=True)
        A.click("send-clipboard")
        A.wait(lambda: A.view() == "clip", "preview")
        meta = A.text("clip-meta")
        assert "320 × 240" in meta, meta
        A.click("clip-send")
        tid = recv("clipboard_video")
        return meta
    video_raw()

    src = work / "src"
    src.mkdir(exist_ok=True)

    @step("File Drop: multiple files → confirmation (names/paths/sizes/count/total/target) → send")
    def files():
        (src / "memo.txt").write_text("hello つて")
        (src / "empty.dat").write_bytes(b"")
        big = src / "medium.bin"
        big.write_bytes(os.urandom(20 * 1024 * 1024))
        A.js(f"window.__tsute.handleDrop({json.dumps([str(src / 'memo.txt'), str(src / 'empty.dat'), str(big), str(src)])}); return 1")
        A.wait(lambda: A.view() == "files", "confirmation view")
        count, total, target = A.text("files-count"), A.text("files-total"), A.text("files-target")
        listing = A.text("files-list")
        assert count == "3" and "20.0 MB" in total and target == "E2E Mac B", (count, total, target)
        assert str(big) in listing and "memo.txt" in listing
        # Drop しただけでは送信されない
        time.sleep(1.5)
        assert incoming_done(B, "files", seen) is None
        A.click("files-send")
        tid = recv("files")
        for name in ("memo.txt", "empty.dat", "medium.bin"):
            assert sha(work / "dl-test-b" / name) == sha(src / name), name
        return f"count={count} total={total}"
    files()

    @step("Single file drop")
    def single():
        one = src / "single.pdf"
        one.write_bytes(os.urandom(300_000))
        A.js(f"window.__tsute.handleDrop({json.dumps([str(one)])}); return 1")
        A.wait(lambda: A.view() == "files", "confirmation")
        A.click("files-send")
        recv("files")
        assert sha(work / "dl-test-b" / "single.pdf") == sha(one)
    single()

    big = src / "big.bin"
    with open(big, "wb") as f:
        for _ in range(args.big_mb):
            f.write(os.urandom(1024 * 1024))

    @step(f"Large file ({args.big_mb}MB): overlap + receiver killed mid-download and restarted")
    def resume_receiver():
        nonlocal B
        A.js(f"window.__tsute.handleDrop({json.dumps([str(big)])}); return 1")
        A.wait(lambda: A.view() == "files", "confirmation")
        A.click("files-send")
        # 受信側で進捗が出始めた（= 送信完了前にダウンロード開始）ことを確認してから kill
        B.wait(lambda: B.js("const p = document.querySelector('#history li[data-direction=incoming][data-status=active] progress'); return p && p.value > 0"), "download started", timeout=120)
        sender_state = A.js("const l = document.querySelector('#history li[data-direction=outgoing]'); return l.dataset.status")
        got = B.js("const p = document.querySelector('#history li[data-direction=incoming][data-status=active] progress'); return p ? [p.value, p.max] : null")
        assert got and got[0] < got[1], f"receiver must be killed mid-download, got {got}"
        B.kill()
        B = apps["test-b"] = launch(binary, "test-b", work)
        # 登録済みプロファイルは仕様どおりウィンドウなし（常駐のみ）で起動するので、確認のために開く
        assert not B.cmd(cmd="window_open"), "enrolled profile must start hidden"
        B.show()
        tid = B.wait(lambda: incoming_done(B, "files", seen), "completed after restart", timeout=600)
        seen.add(tid)
        assert sha(work / "dl-test-b" / "big.bin") == sha(big)
        assert sender_state == "active", f"overlap: sender should still be uploading when receiver downloads (was {sender_state})"
        return f"killed at {got[0]}/{got[1]} bytes; sender was '{sender_state}' (overlap)"
    resume_receiver()

    @step("Large file: sender killed mid-upload and restarted → resumes")
    def resume_sender():
        nonlocal A
        big2 = src / "big2.bin"
        shutil.copy(big, big2)
        A.js(f"window.__tsute.handleDrop({json.dumps([str(big2)])}); return 1")
        A.wait(lambda: A.view() == "files", "confirmation")
        A.click("files-send")
        A.wait(lambda: A.js("const p = document.querySelector('#history li[data-direction=outgoing][data-status=active] progress'); return p && p.value > 0 && p.value < p.max"), "upload in progress", timeout=120)
        A.kill()
        A = apps["test-a"] = launch(binary, "test-a", work)
        assert not A.cmd(cmd="window_open"), "enrolled profile must start hidden"
        A.show()
        tid = B.wait(lambda: incoming_done(B, "files", seen), "completed after sender restart", timeout=600)
        seen.add(tid)
        assert sha(work / "dl-test-b" / "big2.bin") == sha(big2)
    resume_sender()

    @step("Window close keeps process resident and receiving (WebView destroyed)")
    def window_lifecycle():
        B.cmd(cmd="hide")
        B.wait(lambda: not B.cmd(cmd="window_open"), "window destroyed", timeout=10)
        assert B.proc.poll() is None, "process must keep running after window close"
        B.wait(lambda: A.js("return document.body.dataset.connection") == "online", "A online")
        A.wait(lambda: A.js("return document.querySelectorAll('#target option:not([disabled])').length > 0"), "targets")
        pbcopy("received while window closed")
        A.click("send-clipboard")
        A.wait(lambda: A.view() == "clip", "preview")
        A.click("clip-send")
        time.sleep(3)
        B.show()
        tid = B.wait(lambda: incoming_done(B, "clipboard_text", seen), "received while closed")
        seen.add(tid)
        return "ok"
    window_lifecycle()

    @step("Quit from app terminates process")
    def quit_():
        pid = B.proc.pid
        B.cmd(cmd="quit")
        B.proc.wait(timeout=10)
        return f"pid {pid} exited {B.proc.returncode}"
    quit_()


if __name__ == "__main__":
    main()
