"""E2E 用の Windows Clipboard 操作（Win32 API を ctypes で直接呼ぶ。標準ライブラリのみ）。

macOS の pbcopy / osascript に相当する。アプリ（crates/os/src/windows.rs）とは独立に実装し、
「他のアプリから見た Clipboard」として読み書きする。
"""
import ctypes
import struct
import time
from ctypes import wintypes

user32 = ctypes.WinDLL("user32", use_last_error=True)
kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
shell32 = ctypes.WinDLL("shell32", use_last_error=True)

user32.OpenClipboard.argtypes = [wintypes.HWND]
user32.GetClipboardData.restype = wintypes.HANDLE
user32.SetClipboardData.argtypes = [wintypes.UINT, wintypes.HANDLE]
user32.SetClipboardData.restype = wintypes.HANDLE
user32.RegisterClipboardFormatW.argtypes = [wintypes.LPCWSTR]
user32.RegisterClipboardFormatW.restype = wintypes.UINT
user32.IsClipboardFormatAvailable.argtypes = [wintypes.UINT]
kernel32.GlobalAlloc.argtypes = [wintypes.UINT, ctypes.c_size_t]
kernel32.GlobalAlloc.restype = wintypes.HGLOBAL
kernel32.GlobalLock.argtypes = [wintypes.HGLOBAL]
kernel32.GlobalLock.restype = ctypes.c_void_p
kernel32.GlobalUnlock.argtypes = [wintypes.HGLOBAL]
kernel32.GlobalSize.argtypes = [wintypes.HGLOBAL]
kernel32.GlobalSize.restype = ctypes.c_size_t
shell32.DragQueryFileW.argtypes = [wintypes.HANDLE, wintypes.UINT, wintypes.LPWSTR, wintypes.UINT]
shell32.DragQueryFileW.restype = wintypes.UINT

CF_UNICODETEXT = 13
CF_HDROP = 15
GMEM_MOVEABLE = 0x0002


class _Open:
    """Clipboard は同時に 1 プロセスしか開けないため、アプリが開いている間は少し待って再試行する"""

    def __enter__(self):
        for _ in range(100):
            if user32.OpenClipboard(None):
                return self
            time.sleep(0.05)
        raise OSError(f"OpenClipboard failed: {ctypes.get_last_error()}")

    def __exit__(self, *exc):
        user32.CloseClipboard()


def _read(fmt):
    h = user32.GetClipboardData(fmt)
    if not h:
        return None
    p = kernel32.GlobalLock(h)
    try:
        return ctypes.string_at(p, kernel32.GlobalSize(h))
    finally:
        kernel32.GlobalUnlock(h)


def _put(fmt, data):
    h = kernel32.GlobalAlloc(GMEM_MOVEABLE, len(data))
    p = kernel32.GlobalLock(h)
    ctypes.memmove(p, data, len(data))
    kernel32.GlobalUnlock(h)
    if not user32.SetClipboardData(fmt, h):
        raise OSError(f"SetClipboardData failed: {ctypes.get_last_error()}")


def _set(items):
    with _Open():
        user32.EmptyClipboard()
        for fmt, data in items:
            _put(fmt, data)


def get_text():
    with _Open():
        b = _read(CF_UNICODETEXT)
    if b is None:
        return ""
    return b.decode("utf-16-le").split("\0", 1)[0]


def set_text(text):
    _set([(CF_UNICODETEXT, (text + "\0").encode("utf-16-le"))])


def fmt(name):
    return user32.RegisterClipboardFormatW(name)


def has_format(name):
    return bool(user32.IsClipboardFormatAvailable(fmt(name)))


def set_png(path):
    """ブラウザ等と同じく登録形式 "PNG" に PNG のバイト列をそのまま載せる"""
    with open(path, "rb") as f:
        _set([(fmt("PNG"), f.read())])


def set_files(paths):
    """Explorer でのファイルのコピーと同じ CF_HDROP（DROPFILES + NUL 区切りの UTF-16 パス）"""
    body = "".join(str(p) + "\0" for p in paths) + "\0"
    dropfiles = struct.pack("<IiiII", 20, 0, 0, 0, 1)  # pFiles=20, pt=(0,0), fNC=0, fWide=1
    _set([(CF_HDROP, dropfiles + body.encode("utf-16-le"))])


def get_files():
    with _Open():
        h = user32.GetClipboardData(CF_HDROP)
        if not h:
            return []
        n = shell32.DragQueryFileW(h, 0xFFFFFFFF, None, 0)
        out = []
        for i in range(n):
            size = shell32.DragQueryFileW(h, i, None, 0) + 1
            buf = ctypes.create_unicode_buffer(size)
            shell32.DragQueryFileW(h, i, buf, size)
            out.append(buf.value)
        return out
