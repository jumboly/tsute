//! OS 統合。デスクトップアプリはこのモジュールの OS 非依存な型だけを使い、
//! OS ごとの実装差（NSPasteboard / Win32 Clipboard 等）をここに閉じ込める。

use std::path::PathBuf;

use serde::Serialize;

// macOS ではテストからだけ使う（変換ロジックを Windows 以外でも検証するため）
#[cfg(any(windows, test))]
#[cfg_attr(not(windows), allow(dead_code))]
mod dib;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::*;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

/// Send Clipboard 押下時に読み取った内容の候補。1 回のコピーが複数表現を持つことがあるため複数返す（ADR-0003）。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ClipCandidate {
    Text {
        text: String,
    },
    /// PNG に正規化した画像（`path` はアプリが書き出した一時ファイル）、または画像ファイルそのもの
    Image {
        path: PathBuf,
        /// 送信時のファイル名
        name: String,
        mime: String,
        size: u64,
        width: Option<u32>,
        height: Option<u32>,
        /// 元の Clipboard 表現（例: "public.tiff"）や file URL 由来か
        source: String,
        /// アプリが作った一時ファイルか（送信後に削除してよいか）
        temporary: bool,
    },
    Video {
        path: PathBuf,
        name: String,
        mime: String,
        size: u64,
        width: Option<u32>,
        height: Option<u32>,
        duration_ms: Option<u64>,
        source: String,
        temporary: bool,
    },
    /// 動画・画像以外のファイル（Finder でのコピー）
    Files {
        paths: Vec<PathBuf>,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct ClipSnapshot {
    pub candidates: Vec<ClipCandidate>,
    /// 診断用: Clipboard 上の型一覧（内容そのものは含めない）
    pub types: Vec<String>,
}

/// PNG の IHDR から幅・高さを読む（画像デコーダ依存を増やさないため）
pub fn png_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    if data.len() < 24 || &data[..8] != b"\x89PNG\r\n\x1a\n" || &data[12..16] != b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes(data[16..20].try_into().ok()?);
    let h = u32::from_be_bytes(data[20..24].try_into().ok()?);
    Some((w, h))
}

/// 拡張子から判定するファイルの種類（Windows には UTType に当たる仕組みがないため、Clipboard の分類に使う）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Image,
    Video,
}

/// 動画・画像の拡張子と MIME。表にない拡張子は通常のファイルとして扱う（安全側: Files の確認画面になるだけ）
pub fn media_type_for_path(p: &std::path::Path) -> Option<(MediaKind, &'static str)> {
    let ext = p.extension()?.to_str()?.to_ascii_lowercase();
    let t = match ext.as_str() {
        "mp4" => (MediaKind::Video, "video/mp4"),
        "m4v" => (MediaKind::Video, "video/x-m4v"),
        "mov" => (MediaKind::Video, "video/quicktime"),
        "avi" => (MediaKind::Video, "video/x-msvideo"),
        "wmv" => (MediaKind::Video, "video/x-ms-wmv"),
        "mkv" => (MediaKind::Video, "video/x-matroska"),
        "webm" => (MediaKind::Video, "video/webm"),
        "mpg" | "mpeg" => (MediaKind::Video, "video/mpeg"),
        "3gp" => (MediaKind::Video, "video/3gpp"),
        "png" => (MediaKind::Image, "image/png"),
        "jpg" | "jpeg" => (MediaKind::Image, "image/jpeg"),
        "gif" => (MediaKind::Image, "image/gif"),
        "bmp" => (MediaKind::Image, "image/bmp"),
        "webp" => (MediaKind::Image, "image/webp"),
        "heic" => (MediaKind::Image, "image/heic"),
        "heif" => (MediaKind::Image, "image/heif"),
        "tif" | "tiff" => (MediaKind::Image, "image/tiff"),
        _ => return None,
    };
    Some(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_type_by_extension_is_case_insensitive() {
        use std::path::Path;
        assert_eq!(
            media_type_for_path(Path::new(r"C:\v\Clip.MP4")),
            Some((MediaKind::Video, "video/mp4"))
        );
        assert_eq!(
            media_type_for_path(Path::new("a.jpeg")),
            Some((MediaKind::Image, "image/jpeg"))
        );
        assert_eq!(media_type_for_path(Path::new("notes.txt")), None);
        assert_eq!(media_type_for_path(Path::new("noext")), None);
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod unsupported {
    //! macOS / Windows 以外（対象外の OS）でワークスペースをビルドするためのスタブ
    use super::*;
    pub fn read_clipboard(_tmp: &std::path::Path) -> Result<ClipSnapshot, String> {
        Err("clipboard not supported on this OS yet".into())
    }
    pub fn write_text(_t: &str) -> Result<(), String> {
        Err("unsupported".into())
    }
    pub fn write_image_png(_p: &std::path::Path) -> Result<(), String> {
        Err("unsupported".into())
    }
    pub fn write_file_urls(_p: &[PathBuf]) -> Result<(), String> {
        Err("unsupported".into())
    }
}
#[cfg(not(any(target_os = "macos", windows)))]
pub use unsupported::*;
