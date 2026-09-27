//! Clipboard を触らないメディア処理のテスト（常時実行してよい）
#![cfg(target_os = "macos")]

use std::path::PathBuf;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(name)
}

#[test]
fn video_info_and_thumbnail() {
    let p = fixture("video-320x240-2s.mov");
    let (w, h, d) = tsute_os::video_info(&p);
    assert_eq!((w, h), (Some(320), Some(240)));
    assert!((1900..=2100).contains(&d.unwrap()));
    let png = tsute_os::video_thumbnail_png(&p, 160.0).expect("thumbnail");
    let (tw, th) = tsute_os::png_dimensions(&png).unwrap();
    assert!(tw <= 160 && th <= 160 && tw > 0, "{tw}x{th}");
}
