//! 実際の macOS Clipboard（NSPasteboard）を使うテスト。
//!
//! ユーザーの Clipboard を上書きするため既定では無視し、明示実行する:
//!   cargo test -p tsute-os --test clipboard_macos -- --ignored --test-threads=1
//! 実行前のテキストを退避し、終了時に復元する。
#![cfg(target_os = "macos")]

use std::path::PathBuf;
use std::process::Command;

use tsute_os::*;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures").join(name)
}

struct Restore(Option<String>);
impl Drop for Restore {
    fn drop(&mut self) {
        if let Some(t) = &self.0 {
            let _ = write_text(t);
        }
    }
}

fn osascript(script: &str) {
    let st = Command::new("osascript").arg("-e").arg(script).status().unwrap();
    assert!(st.success(), "osascript failed: {script}");
}

#[test]
#[ignore]
fn text_roundtrip_via_pbcopy() {
    let _r = Restore(read_text_for_test());
    let tmp = tempfile::tempdir().unwrap();
    // 他アプリからのコピーを模して pbcopy（別プロセス）で書く
    let mut child = Command::new("pbcopy").stdin(std::process::Stdio::piped()).spawn().unwrap();
    use std::io::Write;
    child.stdin.as_mut().unwrap().write_all("つて test 🌏\n2行目".as_bytes()).unwrap();
    child.wait().unwrap();
    let snap = read_clipboard(tmp.path()).unwrap();
    assert!(matches!(&snap.candidates[0], ClipCandidate::Text { text } if text == "つて test 🌏\n2行目"), "{snap:?}");

    write_text("written by tsute").unwrap();
    let out = Command::new("pbpaste").output().unwrap();
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "written by tsute");
}

#[test]
#[ignore]
fn png_image_roundtrip() {
    let _r = Restore(read_text_for_test());
    let tmp = tempfile::tempdir().unwrap();
    write_image_png(&fixture("image-64x48.png")).unwrap();
    let snap = read_clipboard(tmp.path()).unwrap();
    assert!(snap.types.iter().any(|t| t == "public.png"));
    assert!(snap.types.iter().any(|t| t == "public.tiff"));
    match &snap.candidates[0] {
        ClipCandidate::Image { width, height, mime, path, temporary, .. } => {
            assert_eq!((*width, *height), (Some(64), Some(48)));
            assert_eq!(mime, "image/png");
            assert!(*temporary && path.exists());
        }
        c => panic!("unexpected {c:?}"),
    }
}

#[test]
#[ignore]
fn tiff_only_image_is_normalized_to_png() {
    let _r = Restore(read_text_for_test());
    let tmp = tempfile::tempdir().unwrap();
    let tiff = tmp.path().join("x.tiff");
    let st = Command::new("sips").args(["-s", "format", "tiff"]).arg(fixture("image-64x48.png")).arg("--out").arg(&tiff)
        .stdout(std::process::Stdio::null()).status().unwrap();
    assert!(st.success());
    // AppleScript で TIFF のみを Clipboard に載せる（PNG 表現なし）
    osascript(&format!("set the clipboard to (read (POSIX file \"{}\") as TIFF picture)", tiff.display()));
    let snap = read_clipboard(tmp.path()).unwrap();
    assert!(!snap.types.iter().any(|t| t == "public.png"), "{:?}", snap.types);
    match &snap.candidates[0] {
        ClipCandidate::Image { width, height, source, path, .. } => {
            assert_eq!((*width, *height), (Some(64), Some(48)));
            assert_eq!(source, "public.tiff");
            assert_eq!(png_dimensions(&std::fs::read(path).unwrap()), Some((64, 48)));
        }
        c => panic!("unexpected {c:?}"),
    }
}

#[test]
#[ignore]
fn video_file_url_from_finder_style_copy() {
    let _r = Restore(read_text_for_test());
    let tmp = tempfile::tempdir().unwrap();
    let mov = fixture("video-320x240-2s.mov").canonicalize().unwrap();
    osascript(&format!("set the clipboard to (POSIX file \"{}\")", mov.display()));
    let snap = read_clipboard(tmp.path()).unwrap();
    match &snap.candidates[0] {
        ClipCandidate::Video { path, width, height, duration_ms, temporary, mime, .. } => {
            assert_eq!(path, &mov);
            assert_eq!((*width, *height), (Some(320), Some(240)));
            assert!(duration_ms.unwrap() >= 1900 && duration_ms.unwrap() <= 2100, "{duration_ms:?}");
            assert!(!temporary);
            assert_eq!(mime, "video/quicktime");
        }
        c => panic!("unexpected {c:?} types={:?}", snap.types),
    }
}

#[test]
#[ignore]
fn video_raw_data_on_clipboard() {
    let _r = Restore(read_text_for_test());
    let tmp = tempfile::tempdir().unwrap();
    let bytes = std::fs::read(fixture("video-320x240-2s.mov")).unwrap();
    write_raw_for_test("com.apple.quicktime-movie", &bytes).unwrap();
    let snap = read_clipboard(tmp.path()).unwrap();
    match &snap.candidates[0] {
        ClipCandidate::Video { path, size, width, temporary, .. } => {
            assert_eq!(*size as usize, bytes.len());
            assert_eq!(std::fs::read(path).unwrap(), bytes);
            assert_eq!(*width, Some(320));
            assert!(*temporary);
        }
        c => panic!("unexpected {c:?}"),
    }
}

#[test]
#[ignore]
fn multiple_files_and_write_file_urls() {
    let _r = Restore(read_text_for_test());
    let tmp = tempfile::tempdir().unwrap();
    let a = tmp.path().join("a.txt");
    let b = tmp.path().join("b.bin");
    std::fs::write(&a, "a").unwrap();
    std::fs::write(&b, "b").unwrap();
    write_file_urls(&[a.clone(), b.clone()]).unwrap();
    let snap = read_clipboard(tmp.path()).unwrap();
    match &snap.candidates[0] {
        ClipCandidate::Files { paths } => {
            let canon: Vec<_> = paths.iter().map(|p| p.canonicalize().unwrap()).collect();
            assert_eq!(canon, vec![a.canonicalize().unwrap(), b.canonicalize().unwrap()]);
        }
        c => panic!("unexpected {c:?}"),
    }
    assert_eq!(snap.candidates.len(), 1, "file icon TIFF must not become an image candidate");
}
