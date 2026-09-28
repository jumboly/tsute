//! 実際の Windows Clipboard（Win32）を使うテスト。
//!
//! ユーザーの Clipboard を上書きするため既定では無視し、明示実行する:
//!   cargo test -p tsute-os --test clipboard_windows -- --ignored --test-threads=1
//! 実行前のテキストを退避し、終了時に復元する。
#![cfg(windows)]

use std::path::PathBuf;
use std::process::Command;

use tsute_os::*;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(name)
}

struct Restore(Option<String>);
impl Drop for Restore {
    fn drop(&mut self) {
        if let Some(t) = &self.0 {
            let _ = write_text(t);
        }
    }
}

fn powershell(script: &str) -> String {
    let out = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .output()
        .unwrap();
    assert!(out.status.success(), "powershell failed: {script}: {:?}", out);
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
#[ignore]
fn text_roundtrip_with_other_process() {
    let _r = Restore(read_text_for_test());
    let tmp = tempfile::tempdir().unwrap();
    // 他アプリからのコピーを模して別プロセス（PowerShell の Set-Clipboard）で書く。CRLF は LF にそろうこと
    powershell("Set-Clipboard -Value \"つて test 🌏`r`n2行目\"");
    let snap = read_clipboard(tmp.path()).unwrap();
    assert!(
        matches!(&snap.candidates[0], ClipCandidate::Text { text } if text == "つて test 🌏\n2行目"),
        "{snap:?}"
    );

    // 書き込みは CRLF になり、別プロセスから同じ文字列として読める
    write_text("written by tsute\n2").unwrap();
    let out = powershell("[Console]::OutputEncoding = [Text.Encoding]::UTF8; (Get-Clipboard -Raw)");
    assert_eq!(out.trim_end(), "written by tsute\r\n2");
}

#[test]
#[ignore]
fn image_png_write_then_read() {
    let _r = Restore(read_text_for_test());
    let tmp = tempfile::tempdir().unwrap();
    let png = fixture("image-64x48.png");
    let (w, h) = png_dimensions(&std::fs::read(&png).unwrap()).unwrap();
    write_image_png(&png).unwrap();
    // "PNG" 形式と CF_DIB の両方が載る
    let snap = read_clipboard(tmp.path()).unwrap();
    assert!(snap.types.iter().any(|t| t == "PNG"), "{:?}", snap.types);
    assert!(snap.types.iter().any(|t| t == "CF_DIB"), "{:?}", snap.types);
    let img = snap
        .candidates
        .iter()
        .find_map(|c| match c {
            ClipCandidate::Image {
                width, height, source, ..
            } => Some((*width, *height, source.clone())),
            _ => None,
        })
        .expect("image candidate");
    assert_eq!(img, (Some(w), Some(h), "PNG".to_string()));
    assert!(read_dib_for_test().is_some());
}

#[test]
#[ignore]
fn dib_only_image_is_converted_to_png() {
    let _r = Restore(read_text_for_test());
    let tmp = tempfile::tempdir().unwrap();
    let png = std::fs::read(fixture("image-64x48.png")).unwrap();
    let (w, h) = png_dimensions(&png).unwrap();
    // スクリーンショット（PrintScreen）のように CF_DIB だけが載っている状態を再現する
    write_dib_for_test(&png_to_dib_for_test(&png).unwrap()).unwrap();
    let snap = read_clipboard(tmp.path()).unwrap();
    let ClipCandidate::Image {
        path,
        width,
        height,
        source,
        ..
    } = &snap.candidates[0]
    else {
        panic!("{snap:?}")
    };
    assert_eq!((*width, *height, source.as_str()), (Some(w), Some(h), "CF_DIB"));
    assert!(png_dimensions(&std::fs::read(path).unwrap()).is_some());
}

#[test]
#[ignore]
fn files_roundtrip_via_hdrop() {
    let _r = Restore(read_text_for_test());
    let tmp = tempfile::tempdir().unwrap();
    let a = tmp.path().join("a.txt");
    let b = tmp.path().join("b.txt");
    std::fs::write(&a, "a").unwrap();
    std::fs::write(&b, "b").unwrap();
    write_file_urls(&[a.clone(), b.clone()]).unwrap();
    let snap = read_clipboard(tmp.path()).unwrap();
    assert!(
        matches!(&snap.candidates[..], [ClipCandidate::Files { paths }] if paths == &vec![a.clone(), b.clone()]),
        "{snap:?}"
    );
    // 別プロセス（Explorer と同じ Shell の API）からもファイルとして見える
    let out = powershell(
        "Add-Type -AssemblyName System.Windows.Forms; [System.Windows.Forms.Clipboard]::GetFileDropList() | ForEach-Object { $_ }",
    );
    assert!(out.contains("a.txt") && out.contains("b.txt"), "{out}");

    // 1 件の動画ファイルは Video 候補になる
    let v = tmp.path().join("clip.mov");
    std::fs::copy(fixture("video-320x240-2s.mov"), &v).unwrap();
    write_file_urls(std::slice::from_ref(&v)).unwrap();
    let snap = read_clipboard(tmp.path()).unwrap();
    assert!(
        matches!(&snap.candidates[..], [ClipCandidate::Video { mime, .. }] if mime == "video/quicktime"),
        "{snap:?}"
    );
}
