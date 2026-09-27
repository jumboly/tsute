//! OS 統合。デスクトップアプリはこのモジュールの OS 非依存な型だけを使い、
//! OS ごとの実装差（NSPasteboard / Win32 Clipboard 等）をここに閉じ込める。

use std::path::PathBuf;

use serde::Serialize;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::*;

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

#[cfg(not(target_os = "macos"))]
mod unsupported {
    //! Phase 2 で Windows 実装を追加するまでのビルド用スタブ
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
#[cfg(not(target_os = "macos"))]
pub use unsupported::*;
