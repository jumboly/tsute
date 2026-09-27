#!/usr/bin/env python3
""".app バンドルでのみ有効な OS 統合の確認（Keychain / 通知 / ログイン項目 / Idle 時リソース）。

使い方: python3 e2e/app_checks.py [--app target/release/bundle/macos/Tsute.app]
- ローカル開発サーバーを起動し、Keychain を使うプロファイル（--insecure なし）で登録→再起動→鍵の読み出しを確認する。
- ログイン項目は登録→状態確認→**必ず解除**する（ユーザー環境を元に戻す）。
- 最後に forget_enrollment で Keychain 項目を削除する。
"""
import argparse
import json
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
from driver import App  # noqa: E402
from run_e2e import Local  # noqa: E402

ROOT = Path(__file__).resolve().parent.parent


def rss_mb(pid):
    out = subprocess.run(["ps", "-o", "rss=", "-p", str(pid)], capture_output=True, text=True).stdout.strip()
    return round(int(out) / 1024, 1) if out else None


def webcontent_children(pid):
    # WKWebView の WebContent プロセスは XPC で起動されるため親子関係では辿れない。数だけ参考表示する
    out = subprocess.run(["pgrep", "-f", "com.apple.WebKit.WebContent"], capture_output=True, text=True).stdout.split()
    return len(out)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--app", default=str(ROOT / "target/release/bundle/macos/Tsute.app"))
    args = ap.parse_args()
    binary = Path(args.app) / "Contents/MacOS/tsute"
    work = Path(tempfile.mkdtemp(prefix="tsute-appcheck-"))
    (work / "app").mkdir()
    server = Local(work)
    report = {}
    a = b = None
    try:
        # 送信元（テスト用ファイルストア）と受信先（Keychain）
        a = App(binary, "chk-a", work / "app", ["--insecure-file-credentials", "--download-dir", str(work / "dl-a")]).connect()
        b = App(binary, "chk-kc", work / "app", ["--download-dir", str(work / "dl-b")]).connect()
        for app, name in ((a, "Check A"), (b, "Check Keychain")):
            app.wait(lambda: app.js("return document.body.dataset.ready === '1'"), "ready")
            app.fill("enroll-url", server.base_url)
            app.fill("enroll-key", server.issue_key())
            app.fill("enroll-name", name)
            app.click("enroll-submit")
            app.wait(lambda: app.view() == "main", "main")
        st = b.js("return window.__tsute.state()")
        report["credential_store"] = st["credential_store"]
        assert "Keychain" in st["credential_store"], st
        # Keychain に項目があること（値は表示しない）
        acct = f"chk-kc|{server.base_url}|{st['endpoint_id']}"
        r = subprocess.run(["security", "find-generic-password", "-s", "dev.tsute.desktop", "-a", acct], capture_output=True, text=True)
        report["keychain_item_exists"] = r.returncode == 0

        # 再起動して Keychain から鍵を読めること（同一ビルドなので許可ダイアログは出ない想定）
        b.quit()
        b = App(binary, "chk-kc", work / "app", ["--download-dir", str(work / "dl-b")]).connect()
        time.sleep(1.5)
        report["hidden_on_restart"] = not b.cmd(cmd="window_open")
        b.show()
        b.wait(lambda: b.js("return document.body.dataset.connection") == "online", "online after restart with Keychain key")
        report["reauth_with_keychain_key"] = True

        # 通知: 受信で通知を投稿（ログで確認。表示の見た目は手動確認）
        a.wait(lambda: a.js("return document.querySelectorAll('#target option:not([disabled])').length > 0"), "targets")
        subprocess.run(["pbcopy"], input=b"notification check", check=True)
        a.click("send-clipboard")
        a.wait(lambda: a.view() == "clip", "preview")
        a.click("clip-send")
        b.wait(lambda: b.js("return !!document.querySelector('#history li[data-direction=incoming][data-status=done]')"), "received")
        time.sleep(1)
        log = (work / "app/profiles/chk-kc/logs/tsute.log").read_text()
        report["notification_log"] = [l.split("INFO")[-1].strip() for l in log.splitlines() if "notification" in l.lower()][-3:]

        # ログイン項目: 登録 → 状態 → 解除（必ず戻す）
        try:
            on = b.js("return window.__TAURI__.core.invoke('set_login_item', {enabled: true})")
            report["login_item_after_enable"] = on
        except Exception as e:  # noqa: BLE001
            report["login_item_after_enable"] = f"error: {e}"
        finally:
            try:
                report["login_item_after_disable"] = b.js("return window.__TAURI__.core.invoke('set_login_item', {enabled: false})")
            except Exception as e:  # noqa: BLE001
                report["login_item_after_disable"] = f"error: {e}"

        # Idle リソース: ウィンドウを閉じて（WebView 破棄）落ち着いた後の RSS / CPU
        b.cmd(cmd="hide")
        b.wait(lambda: not b.cmd(cmd="window_open"), "window destroyed")
        time.sleep(8)
        report["idle_rss_mb_window_closed"] = rss_mb(b.proc.pid)
        cpu = subprocess.run(["ps", "-o", "%cpu=", "-p", str(b.proc.pid)], capture_output=True, text=True).stdout.strip()
        report["idle_cpu_percent"] = cpu
        b.show()
        time.sleep(2)
        report["rss_mb_window_open"] = rss_mb(b.proc.pid)

        # 後始末: Keychain 項目を削除
        b.js("return window.__TAURI__.core.invoke('forget_enrollment')")
        r = subprocess.run(["security", "find-generic-password", "-s", "dev.tsute.desktop", "-a", acct], capture_output=True, text=True)
        report["keychain_item_removed"] = r.returncode != 0
    finally:
        for x in (a, b):
            if x:
                x.quit()
        server.stop()
        print(json.dumps(report, ensure_ascii=False, indent=2))
        out = ROOT / "e2e/out"
        out.mkdir(exist_ok=True)
        (out / "app-checks.json").write_text(json.dumps(report, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
