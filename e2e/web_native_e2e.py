#!/usr/bin/env python3
"""Native（macOS デスクトップ）と Web Client の相互運用 E2E。

使い方:
  python3 e2e/web_native_e2e.py --target local
  python3 e2e/web_native_e2e.py --target cloud --env test
  オプション: --binary PATH（既定 target/debug/tsute）

検証内容:
- Native → Web（Text / Image）、Web → Native（Text / Image）が実 UI 操作で届く
- Native の確認画面で、Web が受け取れない種類（動画・ファイル）は理由を表示して送信できない
注意: Native 側は実際の OS Clipboard を使う（run_e2e.py と同じく実行前の内容を退避・復元する）。
Browser（headless Chromium）の Clipboard は OS から独立している。
"""
import argparse
import json
import os
import shutil
import sys
import tempfile
import time
from pathlib import Path

from playwright.sync_api import expect, sync_playwright

sys.path.insert(0, str(Path(__file__).parent))
from run_e2e import FIX, ROOT, Cloud, incoming_done, item_click, launch, osascript, pbcopy, pbpaste  # noqa: E402
from web_e2e import Client, LocalServer, make_png_js, results, step  # noqa: E402

RUN = time.strftime("%H%M%S")


def args_target_is_cloud():
    # クラウドには他の Endpoint も残っているため、先頭がおとりとは限らない
    return "--target" in sys.argv and sys.argv[sys.argv.index("--target") + 1] == "cloud"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--target", choices=["local", "cloud"], default="local")
    ap.add_argument("--env", default="test")
    ap.add_argument("--binary", default=str(ROOT / "target/debug/tsute"))
    args = ap.parse_args()
    work = Path(tempfile.mkdtemp(prefix="tsute-wn-e2e-", dir=os.environ.get("TMPDIR")))
    (work / "app").mkdir()
    saved_clip = pbpaste()
    server = LocalServer(work) if args.target == "local" else Cloud(args.env)
    apps = {}
    ok = True
    try:
        with sync_playwright() as pw:
            browser = pw.chromium.launch()
            try:
                run(browser, server, work, args.binary, apps)
            except Exception:
                ok = False
            finally:
                browser.close()
    finally:
        for a in apps.values():
            a.quit()
        server.stop()
        pbcopy(saved_clip)
        out = ROOT / "e2e/out"
        out.mkdir(exist_ok=True)
        (out / f"web-native-e2e-{args.target}.json").write_text(json.dumps(results, ensure_ascii=False, indent=2))
        shutil.rmtree(work, ignore_errors=True)
        print(f"\n{sum(r['ok'] for r in results)}/{len(results)} steps passed", flush=True)
    sys.exit(0 if ok else 1)


def run(browser, server, work, binary, apps):
    n_name, w_name = f"E2E Mac {RUN}", f"E2E Web {RUN}"
    N = apps["native"] = launch(binary, "test-wn", work)
    W = Client(browser, server.base_url, w_name)
    seen = set()

    def enroll():
        # 一覧の先頭に来るおとり（先に登録）。送信先の初期値が「先頭」ではなく「前回の相手」になることを確かめるため
        decoy = Client(browser, server.base_url, f"E2E Decoy {RUN}")
        decoy.enroll(server.issue_key())
        decoy.ctx.close()
        N.wait(lambda: N.js("return document.body.dataset.ready === '1'"), "ready")
        N.fill("enroll-url", server.base_url)
        N.fill("enroll-key", server.issue_key())
        N.fill("enroll-name", n_name)
        N.click("enroll-submit")
        N.wait(lambda: N.view() == "main", "main view", timeout=30)
        W.enroll(server.issue_key())

    def select_web():
        N.wait(lambda: N.js(f"return [...document.querySelectorAll('#target option')].some(o => o.textContent.includes('{w_name}'))"), "web in targets")
        label = N.js(f"const s = document.getElementById('target'); const o = [...s.options].find(o => o.textContent.includes('{w_name}')); s.value = o.value; return o.textContent")
        assert "（Web）" in label, label
        return label

    def native_to_web_text():
        select_web()
        msg = f"Mac から Web へ {time.time()}"
        pbcopy(msg)
        N.click("send-clipboard")
        N.wait(lambda: N.view() == "clip", "preview")
        assert N.js("return document.getElementById('clip-reject').hidden"), "text must be allowed"
        N.click("clip-send")
        c = W.card(msg)
        expect(c.locator("pre")).to_have_text(msg, timeout=30000)
        c.get_by_role("button", name="コピー").click()
        assert W.page.evaluate("navigator.clipboard.readText()") == msg

    def native_to_web_image():
        select_web()
        osascript(f'set the clipboard to (read (POSIX file "{FIX}/image-64x48.png") as «class PNGf»)')
        N.click("send-clipboard")
        N.wait(lambda: N.view() == "clip", "preview")
        N.click("clip-send")
        img = W.card("画像").locator("img")
        expect(img).to_be_visible(timeout=30000)
        dims = img.evaluate("i => i.decode().then(() => [i.naturalWidth, i.naturalHeight])")
        assert dims == [64, 48], dims
        W.card("画像").get_by_role("button", name="閉じる").click()

    def native_video_rejected():
        # Web は動画・ファイルを受け取れない: 確認画面に理由を出し、送信ボタンを無効にする
        select_web()
        osascript(f'set the clipboard to (POSIX file "{FIX / "video-320x240-2s.mov"}")')
        N.click("send-clipboard")
        N.wait(lambda: N.view() == "clip", "preview")
        why = N.text("clip-reject")
        assert "受け取れません" in why and N.js("return document.querySelector('[data-testid=clip-send]').disabled"), why
        N.click("clip-cancel")
        N.wait(lambda: N.view() == "main", "back to main")
        src = work / "one.txt"
        src.write_text("x")
        N.js(f"window.__tsute.handleDrop({json.dumps([str(src)])}); return 1")
        N.wait(lambda: N.view() == "files", "files confirm")
        why2 = N.text("files-reject")
        assert "受け取れません" in why2 and N.js("return document.querySelector('[data-testid=files-send]').disabled"), why2
        N.click("files-cancel")
        return why

    def web_to_native_text():
        msg = f"スマホの音声入力から Mac へ {time.time()}"
        W.page.fill("#text", msg)
        W.review_and_send(n_name, "テキスト")
        tid = N.wait(lambda: incoming_done(N, "clipboard_text", seen), "native receives text", timeout=60)
        seen.add(tid)
        pbcopy("sentinel")
        time.sleep(0.5)
        assert pbpaste() == "sentinel", "receiving must not overwrite OS clipboard"
        item_click(N, tid, "apply")
        N.wait(lambda: pbpaste() == msg, "applied to OS clipboard")

    def web_to_native_image():
        W.page.evaluate(make_png_js(40, 30))
        W.page.click("#read-clip")
        expect(W.page.locator("#image-preview-meta")).to_contain_text("40×30")
        W.review_and_send(n_name, "画像")
        tid = N.wait(lambda: incoming_done(N, "clipboard_image", seen), "native receives image", timeout=60)
        seen.add(tid)
        item_click(N, tid, "apply")
        N.wait(lambda: "PNGf" in osascript("clipboard info"), "PNG on OS clipboard")

    def remembers_receiver():
        # 再起動直後の送信先は一覧の先頭ではなく、前回送った相手（ここでは Web）になる
        N.quit()
        N2 = apps["native"] = launch(binary, "test-wn", work)
        N2.show()
        N2.wait(lambda: N2.js(f"return [...document.querySelectorAll('#target option')].some(o => o.textContent.includes('{w_name}'))"), "targets loaded")
        sel = N2.js("const s = document.getElementById('target'); return s.options[s.selectedIndex].textContent")
        first = N2.js("return document.getElementById('target').options[0].textContent")
        assert w_name in sel, sel
        assert "Decoy" in first or args_target_is_cloud(), first
        return sel

    for name, fn in [
        ("enroll native + web", enroll), ("native → web text", native_to_web_text),
        ("native → web image", native_to_web_image), ("native video/files rejected for web", native_video_rejected),
        ("web → native text", web_to_native_text), ("web → native image", web_to_native_image),
        ("native remembers last receiver after restart", remembers_receiver),
    ]:
        step(name, fn)
    step("no console errors (web)", lambda: W.check_errors())


if __name__ == "__main__":
    main()
