//! Windows 実装: Win32 Clipboard / WinRT Toast 通知 / Run キーによる自動起動 / Explorer 表示

use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use windows::Data::Xml::Dom::XmlDocument;
use windows::Foundation::TypedEventHandler;
use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};
use windows::Win32::Foundation::{ERROR_SUCCESS, GlobalFree, HANDLE, HGLOBAL, HWND};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData, GetClipboardFormatNameW,
    IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock};
use windows::Win32::System::Ole::{CF_DIB, CF_HDROP, CF_UNICODETEXT};
use windows::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_BINARY, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW,
    RegSetKeyValueW,
};
use windows::Win32::UI::Shell::{DROPFILES, DragQueryFileW, HDROP, SetCurrentProcessExplicitAppUserModelID};
use windows::Win32::UI::WindowsAndMessaging::{CreateWindowExW, HWND_MESSAGE, WINDOW_EX_STYLE, WINDOW_STYLE};
use windows::core::{HSTRING, PCWSTR, w};

use crate::{ClipCandidate, ClipSnapshot, MediaKind, dib, media_type_for_path, png_dimensions};

/// 通知・タスクバーでアプリを識別する AppUserModelID。バンドル ID と同じ値にそろえる
const AUMID: &str = "dev.tsute.desktop";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Clipboard の UTF-16LE（NUL 終端）を文字列にする
fn utf16_until_nul(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&c| u16::from_le_bytes(c))
        .take_while(|&u| u != 0)
        .collect();
    String::from_utf16_lossy(&units)
}

fn write_tmp(tmp: &Path, name: &str, data: &[u8]) -> Result<PathBuf, String> {
    std::fs::create_dir_all(tmp).map_err(|e| e.to_string())?;
    let p = tmp.join(name);
    std::fs::write(&p, data).map_err(|e| e.to_string())?;
    Ok(p)
}

fn stamp() -> String {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}{:03}", d.as_secs(), d.subsec_millis())
}

// ---------------- Clipboard ----------------

thread_local! {
    // Clipboard の所有者にするメッセージ専用ウィンドウ。OpenClipboard(NULL) のあと EmptyClipboard すると
    // 所有者が NULL になり、SetClipboardData が失敗し得る（Win32 の仕様）ため、書き込み用に持つ
    static OWNER: Option<HWND> = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            w!("tsute-clipboard"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            None,
            None,
        )
    }
    .ok();
}

/// 開いている間だけ Clipboard を占有する。Drop で必ず閉じる（閉じ忘れると他アプリがコピーできなくなる）
struct Clipboard;

impl Clipboard {
    fn open() -> Result<Self, String> {
        let owner = OWNER.with(|o| *o);
        // 他アプリが一瞬だけ開いていることがある（Clipboard 履歴・リモートデスクトップ等）ので少し待って再試行する
        for _ in 0..20 {
            if unsafe { OpenClipboard(owner) }.is_ok() {
                return Ok(Clipboard);
            }
            std::thread::sleep(Duration::from_millis(15));
        }
        Err("clipboard is busy (another application has it open)".into())
    }

    fn get(&self, fmt: u32) -> Option<Vec<u8>> {
        unsafe {
            IsClipboardFormatAvailable(fmt).ok()?;
            let h = GetClipboardData(fmt).ok()?;
            let g = HGLOBAL(h.0);
            let size = GlobalSize(g);
            let p = GlobalLock(g) as *const u8;
            if p.is_null() {
                return None;
            }
            let v = std::slice::from_raw_parts(p, size).to_vec();
            let _ = GlobalUnlock(g);
            Some(v)
        }
    }

    fn set(&self, fmt: u32, bytes: &[u8]) -> Result<(), String> {
        unsafe {
            let g = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1)).map_err(|e| e.to_string())?;
            let p = GlobalLock(g) as *mut u8;
            if p.is_null() {
                let _ = GlobalFree(Some(g));
                return Err("GlobalLock failed".into());
            }
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
            // ロック数が 0 になると「失敗」扱いの戻り値になる API なので結果は見ない
            let _ = GlobalUnlock(g);
            // 成功したらメモリの所有権は OS に移る。失敗時だけ自分で解放する
            if let Err(e) = SetClipboardData(fmt, Some(HANDLE(g.0))) {
                let _ = GlobalFree(Some(g));
                return Err(e.to_string());
            }
        }
        Ok(())
    }

    fn empty(&self) -> Result<(), String> {
        unsafe { EmptyClipboard() }.map_err(|e| e.to_string())
    }

    fn formats(&self) -> Vec<u32> {
        let mut v = Vec::new();
        let mut f = 0;
        loop {
            f = unsafe { EnumClipboardFormats(f) };
            if f == 0 {
                break;
            }
            v.push(f);
        }
        v
    }

    fn files(&self) -> Vec<PathBuf> {
        let Ok(h) = (unsafe { GetClipboardData(CF_HDROP.0 as u32) }) else {
            return vec![];
        };
        let hdrop = HDROP(h.0);
        let n = unsafe { DragQueryFileW(hdrop, u32::MAX, None) };
        (0..n)
            .filter_map(|i| {
                let len = unsafe { DragQueryFileW(hdrop, i, None) } as usize;
                let mut buf = vec![0u16; len + 1];
                let got = unsafe { DragQueryFileW(hdrop, i, Some(&mut buf)) } as usize;
                (got > 0).then(|| PathBuf::from(OsString::from_wide(&buf[..got])))
            })
            .collect()
    }
}

impl Drop for Clipboard {
    fn drop(&mut self) {
        let _ = unsafe { CloseClipboard() };
    }
}

fn png_format() -> u32 {
    // Office・ブラウザ・画像編集ソフトが使う登録形式。アルファを保ったまま受け渡せる
    unsafe { RegisterClipboardFormatW(w!("PNG")) }
}

fn format_name(f: u32) -> String {
    let std_name = match f {
        1 => Some("CF_TEXT"),
        2 => Some("CF_BITMAP"),
        7 => Some("CF_OEMTEXT"),
        8 => Some("CF_DIB"),
        13 => Some("CF_UNICODETEXT"),
        15 => Some("CF_HDROP"),
        16 => Some("CF_LOCALE"),
        17 => Some("CF_DIBV5"),
        _ => None,
    };
    if let Some(n) = std_name {
        return n.into();
    }
    let mut buf = [0u16; 256];
    let len = unsafe { GetClipboardFormatNameW(f, &mut buf) };
    if len > 0 {
        String::from_utf16_lossy(&buf[..len as usize])
    } else {
        format!("CF_{f}")
    }
}

fn candidate_for_file(p: &Path) -> Option<ClipCandidate> {
    let meta = std::fs::metadata(p).ok()?;
    if !meta.is_file() {
        return None;
    }
    let (kind, mime) = media_type_for_path(p)?;
    let name = p.file_name()?.to_string_lossy().to_string();
    match kind {
        // 解像度・長さは未取得（Media Foundation 等が必要。表示用の補助情報なので送信は妨げない）
        MediaKind::Video => Some(ClipCandidate::Video {
            path: p.to_path_buf(),
            name,
            mime: mime.into(),
            size: meta.len(),
            width: None,
            height: None,
            duration_ms: None,
            source: "CF_HDROP".into(),
            temporary: false,
        }),
        MediaKind::Image => {
            // 幅・高さはヘッダだけで分かる PNG のみ（画像デコーダを増やさないため）
            let head = std::fs::File::open(p).ok().and_then(|mut f| {
                use std::io::Read;
                let mut b = [0u8; 24];
                f.read_exact(&mut b).ok().map(|_| b)
            });
            let (width, height) = head
                .and_then(|b| png_dimensions(&b))
                .map(|(w, h)| (Some(w), Some(h)))
                .unwrap_or((None, None));
            Some(ClipCandidate::Image {
                path: p.to_path_buf(),
                name,
                mime: mime.into(),
                size: meta.len(),
                width,
                height,
                source: "CF_HDROP".into(),
                temporary: false,
            })
        }
    }
}

/// Send Clipboard 押下時にだけ呼ぶ。Clipboard の常時監視はしない（最重要原則）。
pub fn read_clipboard(tmp: &Path) -> Result<ClipSnapshot, String> {
    let cb = Clipboard::open()?;
    let formats = cb.formats();
    let types: Vec<String> = formats.iter().map(|&f| format_name(f)).collect();
    let mut candidates = Vec::new();

    // 1. Explorer でのファイルのコピー（CF_HDROP）
    let file_paths = if formats.contains(&(CF_HDROP.0 as u32)) {
        cb.files()
    } else {
        vec![]
    };
    if !file_paths.is_empty() {
        match (file_paths.len(), candidate_for_file(&file_paths[0])) {
            (1, Some(c)) => candidates.push(c),
            _ => candidates.push(ClipCandidate::Files {
                paths: file_paths.clone(),
            }),
        }
        return Ok(ClipSnapshot { candidates, types });
    }

    // 2. テキスト。Windows の改行（CRLF）は LF にそろえて送る（受信側の OS に依らず同じ内容にするため）
    if let Some(bytes) = cb.get(CF_UNICODETEXT.0 as u32) {
        let text = utf16_until_nul(&bytes).replace("\r\n", "\n");
        if !text.is_empty() {
            candidates.push(ClipCandidate::Text { text });
        }
    }

    // 3. 画像（"PNG" 形式を優先、なければ CF_DIB を PNG に変換）
    let png_fmt = png_format();
    let (png, source) = match cb.get(png_fmt).filter(|d| png_dimensions(d).is_some()) {
        Some(d) => (Some(d), "PNG"),
        None => (cb.get(CF_DIB.0 as u32).and_then(|d| dib::dib_to_png(&d)), "CF_DIB"),
    };
    drop(cb);
    if let Some(png) = png {
        let (w, h) = png_dimensions(&png)
            .map(|(w, h)| (Some(w), Some(h)))
            .unwrap_or((None, None));
        let name = format!("clipboard-{}.png", stamp());
        let path = write_tmp(tmp, &name, &png)?;
        candidates.push(ClipCandidate::Image {
            path,
            name,
            mime: "image/png".into(),
            size: png.len() as u64,
            width: w,
            height: h,
            source: source.into(),
            temporary: true,
        });
    }
    Ok(ClipSnapshot { candidates, types })
}

pub fn write_text(text: &str) -> Result<(), String> {
    // Windows のアプリは CRLF を前提にするものが多い（古いエディタ等で 1 行につながるのを避ける）
    let normalized = text.replace("\r\n", "\n").replace('\n', "\r\n");
    let bytes: Vec<u8> = wide(&normalized).iter().flat_map(|u| u.to_le_bytes()).collect();
    let cb = Clipboard::open()?;
    cb.empty()?;
    cb.set(CF_UNICODETEXT.0 as u32, &bytes)
}

/// "PNG"（アルファ付き）と CF_DIB（白と合成した 24bpp）の両方を載せる。CF_DIB しか読まないアプリが多いため
pub fn write_image_png(path: &Path) -> Result<(), String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let dibdata = dib::png_to_dib24(&bytes);
    let cb = Clipboard::open()?;
    cb.empty()?;
    let mut ok = false;
    if png_dimensions(&bytes).is_some() {
        ok |= cb.set(png_format(), &bytes).is_ok();
    }
    if let Some(d) = dibdata {
        ok |= cb.set(CF_DIB.0 as u32, &d).is_ok();
    }
    if ok {
        Ok(())
    } else {
        Err("failed to write image to clipboard".into())
    }
}

/// 受信済みファイルを CF_HDROP として載せる。Explorer やメール等へ貼り付けられる
pub fn write_file_urls(paths: &[PathBuf]) -> Result<(), String> {
    let header = DROPFILES {
        pFiles: std::mem::size_of::<DROPFILES>() as u32,
        fWide: true.into(),
        ..Default::default()
    };
    let mut bytes =
        unsafe { std::slice::from_raw_parts(&header as *const _ as *const u8, std::mem::size_of::<DROPFILES>()) }
            .to_vec();
    for p in paths {
        for u in p.as_os_str().encode_wide().chain(std::iter::once(0)) {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
    }
    bytes.extend_from_slice(&[0, 0]);
    let cb = Clipboard::open()?;
    cb.empty()?;
    cb.set(CF_HDROP.0 as u32, &bytes)?;
    // 貼り付け先で「移動」ではなく「コピー」になるよう指定する（受信フォルダのファイルを消さないため）
    let effect = unsafe { RegisterClipboardFormatW(w!("Preferred DropEffect")) };
    let _ = cb.set(effect, &1u32.to_le_bytes());
    Ok(())
}

/// 動画の解像度と長さ。Windows では未実装（表示用の補助情報なので None で送信は妨げない）
pub fn video_info(_path: &Path) -> (Option<u32>, Option<u32>, Option<u64>) {
    (None, None, None)
}

/// 動画のサムネイル。Windows では未実装（プレビューは種類とサイズだけになる）
pub fn video_thumbnail_png(_path: &Path, _max_dim: f64) -> Option<Vec<u8>> {
    None
}

/// Explorer で該当ファイルを選択表示する（複数でも最初の 1 件。受信物は同じフォルダに入るため）
pub fn reveal_in_finder(paths: &[PathBuf]) {
    use std::os::windows::process::CommandExt;
    let Some(p) = paths.first() else { return };
    // explorer は引数を独自に解釈するので、/select,"<path>" をそのまま渡す
    let r = std::process::Command::new("explorer.exe")
        .raw_arg(format!("/select,\"{}\"", p.display()))
        .spawn();
    if let Err(e) = r {
        tracing::warn!(error = %e, "failed to open explorer");
    }
}

// ---------------- レジストリ ----------------

fn reg_get_string(subkey: PCWSTR, value: PCWSTR) -> Option<String> {
    let mut len = 0u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            subkey,
            value,
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut len),
        )
    };
    if r != ERROR_SUCCESS || len == 0 {
        return None;
    }
    let mut buf = vec![0u16; (len as usize).div_ceil(2)];
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            subkey,
            value,
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut len),
        )
    };
    if r != ERROR_SUCCESS {
        return None;
    }
    let s: Vec<u16> = buf.into_iter().take_while(|&u| u != 0).collect();
    Some(String::from_utf16_lossy(&s))
}

fn reg_set_string(subkey: PCWSTR, value: PCWSTR, data: &str) -> Result<(), String> {
    let w = wide(data);
    let r = unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            subkey,
            value,
            REG_SZ.0,
            Some(w.as_ptr().cast()),
            (w.len() * 2) as u32,
        )
    };
    if r == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(format!("registry write failed ({})", r.0))
    }
}

fn reg_delete(subkey: PCWSTR, value: PCWSTR) {
    let _ = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, subkey, value) };
}

// ---------------- 通知 ----------------

static NOTIFY_CLICK: OnceLock<Box<dyn Fn(String) + Send + Sync>> = OnceLock::new();
/// 表示した通知を少しの間保持する。破棄されるとクリック（Activated）のハンドラが呼ばれなくなるため
static LIVE: Mutex<Vec<ToastNotification>> = Mutex::new(Vec::new());

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// 一度呼ぶ。インストーラを使わない（MSIX でない）アプリが Toast を出すには、AUMID を
/// HKCU\Software\Classes\AppUserModelId に登録する必要がある。通知クリックで `on_click(通知 ID)` が呼ばれる。
pub fn init_notifications(on_click: impl Fn(String) + Send + Sync + 'static) -> bool {
    let _ = NOTIFY_CLICK.set(Box::new(on_click));
    let key = HSTRING::from(format!("Software\\Classes\\AppUserModelId\\{AUMID}"));
    if let Err(e) = reg_set_string(PCWSTR(key.as_ptr()), w!("DisplayName"), "つて") {
        tracing::warn!(error = %e, "failed to register AUMID; OS notifications disabled");
        return false;
    }
    if let Err(e) = unsafe { SetCurrentProcessExplicitAppUserModelID(&HSTRING::from(AUMID)) } {
        tracing::warn!(error = %e, "SetCurrentProcessExplicitAppUserModelID failed");
    }
    true
}

pub fn notify(id: &str, title: &str, body: &str) {
    let r = (|| -> windows::core::Result<()> {
        let xml = format!(
            "<toast launch=\"{}\"><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text></binding></visual></toast>",
            xml_escape(id),
            xml_escape(title),
            xml_escape(body)
        );
        let doc = XmlDocument::new()?;
        doc.LoadXml(&HSTRING::from(xml))?;
        let toast = ToastNotification::CreateToastNotification(&doc)?;
        let id2 = id.to_string();
        toast.Activated(&TypedEventHandler::new(move |_, _| {
            if let Some(cb) = NOTIFY_CLICK.get() {
                cb(id2.clone());
            }
            Ok(())
        }))?;
        ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(AUMID))?.Show(&toast)?;
        let mut live = LIVE.lock().expect("lock");
        live.push(toast);
        if live.len() > 20 {
            live.remove(0);
        }
        Ok(())
    })();
    if let Err(e) = r {
        tracing::warn!(error = %e, "toast notification failed");
    }
}

// ---------------- ログイン時の自動起動 ----------------

const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
/// 設定 > アプリ > スタートアップ / タスク マネージャーでユーザーが無効にした状態が記録される場所
const APPROVED_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\StartupApproved\\Run");
const RUN_VALUE: PCWSTR = w!("Tsute");

fn run_command() -> Option<String> {
    // 引数なし = default プロファイル。自動起動は default のみ（ADR-0012）
    std::env::current_exe().ok().map(|p| format!("\"{}\"", p.display()))
}

/// HKCU の Run キー（管理者権限不要）。「設定 > アプリ > スタートアップ」とタスク マネージャーに表示され、
/// ユーザーは OS 側でも無効にできる
pub fn login_item_status() -> &'static str {
    let Some(expected) = run_command() else {
        return "unavailable";
    };
    match reg_get_string(RUN_KEY, RUN_VALUE) {
        // 別の場所の exe（移動・再インストール前）を指す値は無効とみなす。有効にし直すと上書きされる
        Some(v) if v.eq_ignore_ascii_case(&expected) => {}
        _ => return "not_registered",
    }
    let mut data = [0u8; 12];
    let mut len = data.len() as u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            APPROVED_KEY,
            RUN_VALUE,
            RRF_RT_REG_BINARY,
            None,
            Some(data.as_mut_ptr().cast()),
            Some(&mut len),
        )
    };
    // 先頭バイトが奇数（0x03 等）なら OS 側で無効化されている
    if r == ERROR_SUCCESS && len > 0 && data[0] & 1 == 1 {
        "disabled_by_user"
    } else {
        "enabled"
    }
}

pub fn set_login_item(enabled: bool) -> Result<(), String> {
    if enabled {
        let cmd = run_command().ok_or("cannot resolve executable path")?;
        reg_set_string(RUN_KEY, RUN_VALUE, &cmd)?;
        // アプリの設定で明示的に有効にした = OS 側の「無効」も解除してよい
        reg_delete(APPROVED_KEY, RUN_VALUE);
    } else {
        reg_delete(RUN_KEY, RUN_VALUE);
        reg_delete(APPROVED_KEY, RUN_VALUE);
    }
    Ok(())
}

/// タスクバーが明るいテーマか。通知領域のアイコンは単色なので、背景に合わせて黒/白を選ぶために使う
pub fn taskbar_is_light() -> bool {
    let mut v = 0u32;
    let mut len = 4u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("SystemUsesLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some((&mut v as *mut u32).cast()),
            Some(&mut len),
        )
    };
    // 値がない古い Windows 10 はタスクバーが暗い
    r == ERROR_SUCCESS && v != 0
}

/// テスト専用: 任意の登録形式で生データを Clipboard に載せる
#[doc(hidden)]
pub fn write_raw_for_test(format: &str, bytes: &[u8]) -> Result<(), String> {
    let f = unsafe { RegisterClipboardFormatW(&HSTRING::from(format)) };
    let cb = Clipboard::open()?;
    cb.empty()?;
    cb.set(f, bytes)
}

/// テスト専用: 現在の Clipboard のテキスト（退避・復元用）
#[doc(hidden)]
pub fn read_text_for_test() -> Option<String> {
    let cb = Clipboard::open().ok()?;
    let bytes = cb.get(CF_UNICODETEXT.0 as u32)?;
    Some(utf16_until_nul(&bytes))
}

/// テスト専用: CF_DIB を載せる（スクリーンショット等、PNG 形式を伴わない画像のコピーを再現するため）
#[doc(hidden)]
pub fn write_dib_for_test(dib_bytes: &[u8]) -> Result<(), String> {
    let cb = Clipboard::open()?;
    cb.empty()?;
    cb.set(CF_DIB.0 as u32, dib_bytes)
}

/// テスト専用: Clipboard の画像を CF_DIB として読む
#[doc(hidden)]
pub fn read_dib_for_test() -> Option<Vec<u8>> {
    Clipboard::open().ok()?.get(CF_DIB.0 as u32)
}

/// テスト専用: 外部から DIB を組み立てる（dib モジュールは非公開のため）
#[doc(hidden)]
pub fn png_to_dib_for_test(png: &[u8]) -> Option<Vec<u8>> {
    dib::png_to_dib24(png)
}
