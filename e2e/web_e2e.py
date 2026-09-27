#!/usr/bin/env python3
"""Web / PWA Client の E2E（Playwright。ADR-0015）。

使い方:
  python3 e2e/web_e2e.py --target local                # ローカル開発サーバー（web/ を /app/ で配信）
  python3 e2e/web_e2e.py --target cloud --env test     # デプロイ済み環境（infra/.build/test.json）
  オプション: --headed（画面を表示）, --only chromium|webkit

2 つの独立した Browser コンテキスト（= 2 つの Web Endpoint）で、実際の UI 操作により
Enrollment → 送信先選択 → Preview → Send → 受信 → 明示的 Copy を検証する。
Browser の Clipboard はコンテキストごとに独立で、ユーザーの OS Clipboard は使わない（headless のため）。
スマホ実機・OS の音声入力・Web Push の表示・Share Sheet は自動化できないため手動確認項目（docs/TESTING.md）。
"""
import argparse
import json
import sys
import tempfile
import threading
import time
import traceback
from pathlib import Path

from playwright.sync_api import expect, sync_playwright

sys.path.insert(0, str(Path(__file__).parent))
from run_e2e import Cloud, ROOT  # noqa: E402

results = []


def step(name, fn):
    t0 = time.time()
    print(f"--- {name}", flush=True)
    try:
        detail = fn()
        results.append({"step": name, "ok": True, "secs": round(time.time() - t0, 1), "detail": detail})
        print(f"    OK ({time.time() - t0:.1f}s) {detail or ''}", flush=True)
    except Exception as e:
        results.append({"step": name, "ok": False, "secs": round(time.time() - t0, 1), "error": str(e)})
        print(f"    FAIL: {e}", flush=True)
        traceback.print_exc()
        raise


class LocalServer:
    """ログのパイプが詰まってサーバーが止まらないよう、標準出力を読み続ける"""

    def __init__(self, work):
        import subprocess
        import urllib.request
        self._urlreq = urllib.request
        self.dir = work / "server"
        subprocess.run(["cargo", "build", "-q", "-p", "tsute-server-local"], cwd=ROOT, check=True)
        self.proc = subprocess.Popen(
            [str(ROOT / "target/debug/tsute-devserver"), "--bind", "127.0.0.1:0", "--data-dir", str(self.dir),
             "--web-dir", str(ROOT / "web")],
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, cwd=ROOT)
        self.base_url = None
        for line in self.proc.stdout:
            if line.startswith("TSUTE_DEVSERVER_URL="):
                # 127.0.0.1 は secure context（Web Crypto / Service Worker が使える）。presigned URL も同じ
                # オリジンを指すので localhost に置き換えない（置き換えるとクロスオリジンになる）
                self.base_url = line.strip().split("=", 1)[1]
                break
        if not self.base_url:
            raise RuntimeError("devserver did not start")
        threading.Thread(target=lambda: [None for _ in self.proc.stdout], daemon=True).start()
        self.token = (self.dir / "admin-token").read_text()

    def issue_key(self):
        req = self._urlreq.Request(f"{self.base_url}/admin/enrollment-keys", method="POST",
                                   headers={"x-admin-token": self.token})
        return json.load(self._urlreq.urlopen(req))["enrollment_key"]

    def stop(self):
        self.proc.terminate()


class Client:
    """1 つの Web Endpoint（Browser コンテキスト）"""

    def __init__(self, browser, base_url, name, clipboard=True):
        self.name = name
        self.base_url = base_url
        perms = ["clipboard-read", "clipboard-write"] if clipboard else []
        self.ctx = browser.new_context(permissions=perms, base_url=base_url)
        self.errors = []
        # 意図的なリロード・クローズで中断された fetch を WebKit は "access control checks" としてエラー出力するため、
        # その間のメッセージは数えない
        self.navigating = False
        self.page = None
        self.open()

    def open(self):
        self.page = self.ctx.new_page()
        # CSP 違反や未捕捉例外があれば失敗扱いにする（インライン script / 外部読み込みの混入を検出）
        self.page.on("console", lambda m: m.type == "error" and not self.navigating and self.errors.append(m.text))
        self.page.on("pageerror", lambda e: not self.navigating and self.errors.append(str(e)))
        self.page.goto("/app/")
        return self.page

    def close_page(self):
        self.navigating = True
        self.page.close()
        self.page = None
        self.navigating = False

    def reload(self):
        self.navigating = True
        self.page.reload()
        self.page.wait_for_load_state("load")
        self.navigating = False

    def enroll(self, key):
        p = self.page
        expect(p.locator("#view-enroll")).to_be_visible(timeout=15000)
        p.fill("#enroll-name", self.name)
        p.fill("#enroll-key", key)
        p.click("#enroll-submit")
        expect(p.locator("#view-main")).to_be_visible(timeout=15000)
        expect(p.locator("#me-name")).to_have_text(self.name)

    def endpoint_id(self):
        return self.page.evaluate("""async () => (await import('./lib/identity.js')).load().then(i => i.endpointId)""")

    def select_target(self, name):
        lab = self.page.locator("#targets label", has_text=name)
        expect(lab).to_be_visible(timeout=15000)
        lab.locator("input").check()

    def review_and_send(self, target, expect_kind):
        p = self.page
        self.select_target(target)
        expect(p.locator("#review")).to_be_enabled()
        p.click("#review")
        d = p.locator("#confirm")
        expect(d).to_be_visible()
        expect(p.locator("#confirm-target")).to_have_text(target)
        expect(p.locator("#confirm-kind")).to_contain_text(expect_kind)
        p.click("#confirm-send")
        expect(p.locator("#toast")).to_contain_text("送信しました", timeout=30000)

    def card(self, text=None):
        loc = self.page.locator("#inbox li")
        return loc.filter(has_text=text) if text else loc

    def check_errors(self):
        errs = [e for e in self.errors if "favicon" not in e]
        if errs:
            raise AssertionError(f"{self.name}: console errors: {errs}")


def make_png_js(w, h):
    return f"""async () => {{
      const c = document.createElement('canvas'); c.width = {w}; c.height = {h};
      const g = c.getContext('2d'); g.fillStyle = '#d33'; g.fillRect(0, 0, {w}, {h});
      g.fillStyle = '#33d'; g.fillRect(0, 0, {w // 2}, {h // 2});
      const b = await new Promise(r => c.toBlob(r, 'image/png'));
      await navigator.clipboard.write([new ClipboardItem({{'image/png': b}})]);
      return b.size;
    }}"""


def run(pw, engine, server, headed):
    browser = getattr(pw, engine).launch(headless=not headed)
    tag = f"E2E {engine}"
    # WebKit の Playwright は clipboard 権限を付与できないため、Clipboard の読み書きは Chromium だけで検証する
    clip = engine == "chromium"
    a = Client(browser, server.base_url, f"{tag} A", clipboard=clip)
    b = Client(browser, server.base_url, f"{tag} B", clipboard=clip)
    P = lambda s: f"[{engine}] {s}"  # noqa: E731

    def enroll():
        k = server.issue_key()
        a.enroll(k)
        b.enroll(server.issue_key())
        # 使用済み Enrollment Key の再利用は拒否（リプレイ防止）
        x = Client(browser, server.base_url, f"{tag} X", clipboard=False)
        x.page.fill("#enroll-name", "reuse")
        x.page.fill("#enroll-key", k)
        x.page.click("#enroll-submit")
        expect(x.page.locator("#enroll-error")).to_contain_text("無効", timeout=10000)
        x.ctx.close()
        return {"a": a.endpoint_id(), "b": b.endpoint_id()}

    def endpoints():
        lab = a.page.locator("#targets label", has_text=f"{tag} B")
        expect(lab).to_contain_text("Web", timeout=15000)
        # B は WebSocket（ticket 認証）で接続中
        expect(lab).to_contain_text("オンライン", timeout=15000)
        expect(b.page.locator("body")).to_have_attribute("data-connection", "online", timeout=15000)

    def text_live():
        msg = f"音声入力のテキスト {time.time()}\n二行目 <b>not html</b>"
        a.page.fill("#text", msg)
        expect(a.page.locator("#payload-meta")).to_contain_text("テキスト")
        a.review_and_send(f"{tag} B", "テキスト")
        # B はリロードせずに WS 通知で受け取る
        c = b.card("音声入力のテキスト")
        expect(c).to_be_visible(timeout=15000)
        expect(c.locator("pre")).to_have_text(msg)
        # 受信内容は textContent で描画され、HTML として解釈されない
        assert c.locator("pre b").count() == 0
        if clip:
            c.get_by_role("button", name="コピー").click()
            expect(b.page.locator("#toast")).to_contain_text("コピーしました")
            got = b.page.evaluate("navigator.clipboard.readText()")
            assert got == msg, got
        else:
            c.get_by_role("button", name="閉じる").click()
        # 送信側の履歴が「受信済み」になる（明示操作で received）
        expect(a.page.locator("#history li").first).to_contain_text("受信済み", timeout=15000)

    def no_auto_send():
        # 入力しただけでは送らない: 確認画面で「戻る」を押すと何も作られない
        before = a.page.locator("#history li").count()
        a.page.fill("#text", "送らないテキスト")
        a.select_target(f"{tag} B")
        a.page.click("#review")
        a.page.locator("#confirm").get_by_role("button", name="戻る").click()
        time.sleep(1.5)
        assert b.card("送らないテキスト").count() == 0
        assert a.page.locator("#history li").count() == before
        a.page.click("#clear")

    def big_text():
        msg = ("長いテキスト" * 8000) + "END"  # 64KiB 超 → ファイルとして chunk 転送
        a.page.fill("#text", msg)
        a.review_and_send(f"{tag} B", "テキスト")
        c = b.card("長いテキスト")
        expect(c.locator("pre")).to_be_visible(timeout=30000)
        got = c.locator("pre").text_content()
        assert got == msg, len(got)
        c.get_by_role("button", name="閉じる").click()
        return {"bytes": len(msg.encode())}

    def image():
        if clip:
            size = a.page.evaluate(make_png_js(64, 48))
            a.page.click("#read-clip")
        else:
            # Clipboard 権限なし: Paste 経路（ClipboardEvent に画像 File を載せる）で入れる
            size = a.page.evaluate("""async () => {
              const c = document.createElement('canvas'); c.width = 64; c.height = 48;
              c.getContext('2d').fillRect(0, 0, 64, 48);
              const b = await new Promise(r => c.toBlob(r, 'image/png'));
              const dt = new DataTransfer(); dt.items.add(new File([b], 'x.png', {type: 'image/png'}));
              document.getElementById('text').dispatchEvent(new ClipboardEvent('paste', {clipboardData: dt, bubbles: true, cancelable: true}));
              return b.size;
            }""")
        expect(a.page.locator("#image-preview")).to_be_visible(timeout=10000)
        expect(a.page.locator("#image-preview-meta")).to_contain_text("64×48")
        a.review_and_send(f"{tag} B", "画像")
        c = b.card("画像")
        img = c.locator("img")
        expect(img).to_be_visible(timeout=30000)
        dims = img.evaluate("i => i.decode().then(() => [i.naturalWidth, i.naturalHeight])")
        assert dims == [64, 48], dims
        if clip:
            c.get_by_role("button", name="コピー").click()
            expect(b.page.locator("#toast")).to_contain_text("コピーしました")
            types = b.page.evaluate("navigator.clipboard.read().then(items => items.flatMap(i => i.types))")
            assert "image/png" in types, types
        c.get_by_role("button", name="閉じる").click()
        return {"png_bytes": size}

    def jpeg_normalized():
        if not clip:
            return "skip (no clipboard permission)"
        a.page.evaluate("""async () => {
          const c = document.createElement('canvas'); c.width = 30; c.height = 20;
          c.getContext('2d').fillRect(0, 0, 30, 20);
          const b = await new Promise(r => c.toBlob(r, 'image/jpeg'));
          const dt = new DataTransfer(); dt.items.add(new File([b], 'x.jpg', {type: 'image/jpeg'}));
          document.getElementById('text').dispatchEvent(new ClipboardEvent('paste', {clipboardData: dt, bubbles: true, cancelable: true}));
        }""")
        expect(a.page.locator("#image-preview-meta")).to_contain_text("image/jpeg から変換", timeout=10000)
        a.page.click("#image-remove")

    def offline_recovery():
        # B を閉じている間に送る（WS なし）→ 開き直すと HTTP から未受信を回収する
        bid = b.endpoint_id()
        b.close_page()
        lab = a.page.locator("#targets label", has_text=f"{tag} B")
        expect(lab).to_contain_text("オフライン", timeout=20000)
        a.page.fill("#text", "オフライン中に送ったテキスト")
        a.review_and_send(f"{tag} B", "テキスト")
        b.open()
        expect(b.page.locator("#view-main")).to_be_visible(timeout=15000)
        # 再 Open 後も同じ Endpoint
        assert b.endpoint_id() == bid
        c = b.card("オフライン中に送ったテキスト")
        expect(c).to_be_visible(timeout=15000)
        # 操作するまでは Backend に残る: リロードしても消えない
        b.reload()
        c = b.card("オフライン中に送ったテキスト")
        expect(c).to_be_visible(timeout=15000)
        c.get_by_role("button", name="閉じる").click()
        # received の POST 完了後にカードが消える。完了前にリロードすると中断されて再表示されるため待つ
        expect(c).to_have_count(0, timeout=15000)
        b.reload()
        expect(b.page.locator("#view-main")).to_be_visible(timeout=15000)
        time.sleep(1.5)
        assert b.card("オフライン中に送ったテキスト").count() == 0

    def capability():
        # 3 つ目の Endpoint を「Native 相当（全種類を受信）」に申告させ、Web(B) が Video / Files を受け取れないことを確認
        n = Client(browser, server.base_url, f"{tag} N", clipboard=False)
        n.enroll(server.issue_key())
        r = n.page.evaluate(f"""async () => {{
          const {{ Api }} = await import('./lib/api.js');
          const id = await (await import('./lib/identity.js')).load();
          const api = new Api(id);
          await api.setCapabilities(['clipboard_text','clipboard_image','clipboard_video','files']);
          const eps = await api.endpoints();
          const b = eps.find(e => e.name === '{tag} B');
          const out = {{ accepts: b.accepts, kind: b.client_kind }};
          for (const kind of ['clipboard_video', 'files']) {{
            try {{
              await api.request('POST', '/api/transfers', {{ receiver: b.endpoint_id, kind,
                files: [{{ name: 'v.mov', size: 10, mime: 'video/quicktime' }}] }});
              out[kind] = 'accepted';
            }} catch (e) {{ out[kind] = e.status + ':' + e.code; }}
          }}
          return out;
        }}""")
        assert r["kind"] == "web" and sorted(r["accepts"]) == ["clipboard_image", "clipboard_text"], r
        assert r["clipboard_video"] == "422:receiver_cannot_accept", r
        assert r["files"] == "422:receiver_cannot_accept", r
        n.ctx.close()
        return r

    def share_target():
        # Share Target（manifest の POST）を Service Worker が受けて送信画面の Preview に入れる。送信はしない
        a.page.evaluate("navigator.serviceWorker.ready")
        a.reload()
        a.page.wait_for_function("navigator.serviceWorker.controller !== null", timeout=10000)
        a.page.evaluate("""() => {
          const f = document.createElement('form');
          f.method = 'POST'; f.action = 'share'; f.enctype = 'multipart/form-data';
          for (const [k, v] of [['title', '共有タイトル'], ['text', '共有テキスト'], ['url', 'https://example.test/x']]) {
            const i = document.createElement('input'); i.type = 'hidden'; i.name = k; i.value = v; f.append(i);
          }
          document.body.append(f); f.submit();
        }""")
        expect(a.page.locator("#toast")).to_contain_text("共有された内容を読み込みました", timeout=15000)
        v = a.page.input_value("#text")
        assert v == "共有タイトル\n共有テキスト\nhttps://example.test/x", v
        assert "share" not in a.page.url
        a.page.click("#clear")

    def pwa():
        info = a.page.evaluate("""async () => {
          const r = await navigator.serviceWorker.ready;
          const m = await (await fetch('manifest.webmanifest')).json();
          return { scope: r.scope, display: m.display, start: m.start_url, icons: m.icons.length,
                   share: !!m.share_target, controller: !!navigator.serviceWorker.controller };
        }""")
        assert info["scope"].endswith("/app/"), info
        assert info["display"] == "standalone" and info["icons"] >= 3, info
        # Push の UI（VAPID が設定されていればボタン、無ければ理由の表示）
        a.page.click("#open-settings")
        expect(a.page.locator("#push-status")).not_to_be_empty(timeout=10000)
        info["push_status"] = a.page.locator("#push-status").text_content()
        a.page.click("#close-settings")
        return info

    def headers():
        r = a.page.request.get("/app/")
        csp = r.headers.get("content-security-policy", "")
        assert "script-src 'self'" in csp and "frame-ancestors 'none'" in csp, csp
        r = a.page.request.get("/api/endpoints")
        assert r.status == 401
        # Cookie を使わない（ambient credential が無いので CSRF が成立しない）
        assert not a.ctx.cookies(), a.ctx.cookies()

    steps = [
        ("enroll", enroll), ("endpoints", endpoints), ("text_live", text_live), ("no_auto_send", no_auto_send),
        ("big_text", big_text), ("image", image), ("jpeg_normalized", jpeg_normalized),
        ("offline_recovery", offline_recovery), ("capability", capability), ("share_target", share_target),
        ("pwa", pwa), ("headers", headers),
    ]
    try:
        for n, fn in steps:
            step(P(n), fn)
        step(P("no_console_errors"), lambda: (a.check_errors(), b.check_errors()) and None)
    finally:
        browser.close()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--target", choices=["local", "cloud"], default="local")
    ap.add_argument("--env", default="test")
    ap.add_argument("--headed", action="store_true")
    ap.add_argument("--only", choices=["chromium", "webkit"])
    args = ap.parse_args()
    work = Path(tempfile.mkdtemp(prefix="tsute-web-e2e-"))
    server = LocalServer(work) if args.target == "local" else Cloud(args.env)
    ok = True
    try:
        with sync_playwright() as pw:
            for engine in ([args.only] if args.only else ["chromium", "webkit"]):
                try:
                    run(pw, engine, server, args.headed)
                except Exception:
                    ok = False
    finally:
        server.stop()
        out = ROOT / "e2e/out"
        out.mkdir(exist_ok=True)
        (out / f"web-e2e-{args.target}.json").write_text(json.dumps(results, ensure_ascii=False, indent=2))
        passed = sum(r["ok"] for r in results)
        print(f"\n{passed}/{len(results)} steps passed", flush=True)
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
