"""つて デスクトップアプリの E2E ドライバ（ADR-0013）。

アプリを `--automation`（+ TSUTE_AUTOMATION=1）で起動し、プロファイル内の Unix ソケット（Windows は名前付きパイプ）経由で
WebView の DOM を操作する。ボタンのクリック → Tauri コマンド → client-core → Cloud という本番と同じ経路を通る。
外部依存なし（標準ライブラリのみ）。
"""
import json
import os
import socket
import subprocess
import time
from pathlib import Path


class _Pipe:
    """Windows の名前付きパイプをソケットと同じ sendall / recv で扱う"""

    def __init__(self, path):
        self._f = open(path, "r+b", buffering=0)

    def sendall(self, data):
        self._f.write(data)

    def recv(self, n):
        return self._f.read(n)


class App:
    def __init__(self, binary, profile, data_dir, extra_args=(), env=None):
        self.profile = profile
        self.data_dir = Path(data_dir)
        self.sock_path = self.data_dir / "profiles" / profile / "automation.sock"
        e = dict(os.environ)
        e["TSUTE_AUTOMATION"] = "1"
        e.setdefault("RUST_LOG", "info")
        if env:
            e.update(env)
        args = [str(binary), "--profile", profile, "--data-dir", str(self.data_dir), "--automation", *extra_args]
        self.log = open(self.data_dir / f"{profile}.stdout.log", "ab")
        self.proc = subprocess.Popen(args, env=e, stdout=self.log, stderr=subprocess.STDOUT)
        self._sock = None
        self._buf = b""

    def connect(self, timeout=30):
        deadline = time.time() + timeout
        while time.time() < deadline:
            if self.proc.poll() is not None:
                raise RuntimeError(f"{self.profile}: app exited with {self.proc.returncode}")
            # アプリはパス長上限を超える場合に別の場所へソケットを作り、そのパスを .sock.path に書く
            path_file = self.sock_path.with_suffix(".sock.path")
            if path_file.exists():
                self.sock_path = Path(path_file.read_text())
            if os.name == "nt" and path_file.exists():
                try:
                    self._sock = _Pipe(str(self.sock_path))
                    return self
                except OSError:
                    pass
            elif self.sock_path.exists():
                try:
                    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                    s.connect(str(self.sock_path))
                    self._sock = s
                    return self
                except OSError:
                    pass
            time.sleep(0.2)
        raise TimeoutError(f"{self.profile}: automation socket not ready")

    def cmd(self, **req):
        self._sock.sendall((json.dumps(req) + "\n").encode())
        while b"\n" not in self._buf:
            chunk = self._sock.recv(65536)
            if not chunk:
                raise RuntimeError(f"{self.profile}: socket closed")
            self._buf += chunk
        line, self._buf = self._buf.split(b"\n", 1)
        r = json.loads(line)
        if not r["ok"]:
            raise RuntimeError(f"{self.profile}: {req.get('cmd')} failed: {r['error']}")
        return r["value"]

    def js(self, code):
        return self.cmd(cmd="eval", js=code)

    def show(self, view=None):
        self.cmd(cmd="show", view=view)
        self.wait(lambda: self.cmd(cmd="window_open"), "window open")
        self.wait(lambda: self.js("return document.body.dataset.ready === '1'"), "ui ready")

    def wait(self, pred, what, timeout=60, interval=0.3):
        deadline = time.time() + timeout
        last = None
        while time.time() < deadline:
            try:
                v = pred()
                if v:
                    return v
            except Exception as e:  # noqa: BLE001 — 待機中の一時的失敗は再試行する
                last = e
            time.sleep(interval)
        raise TimeoutError(f"{self.profile}: timed out waiting for {what} (last error: {last})")

    # ---- UI 操作ヘルパ（実際の DOM 要素をクリック・入力する） ----
    def click(self, testid):
        self.js(f"""const e = document.querySelector('[data-testid="{testid}"]');
                    if (!e) throw new Error('no element {testid}');
                    if (e.disabled) throw new Error('disabled {testid}');
                    e.click(); return true;""")

    def fill(self, testid, value):
        self.js(f"""const e = document.querySelector('[data-testid="{testid}"]');
                    e.value = {json.dumps(value)}; e.dispatchEvent(new Event('input')); return true;""")

    def text(self, testid):
        return self.js(f"""const e = document.querySelector('[data-testid="{testid}"]'); return e ? e.textContent : null;""")

    def view(self):
        return self.js("return document.body.dataset.view")

    def quit(self):
        try:
            self.cmd(cmd="quit")
        except Exception:  # noqa: BLE001
            pass
        try:
            self.proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.proc.kill()

    def kill(self):
        """異常終了（プロセス kill）の再現"""
        self.proc.kill()
        self.proc.wait()
