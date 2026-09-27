//! 実クラウドで presigned PUT の成否をファイル種別ごとに確かめる診断ツール。
//! 使い方: cargo run -p tsute-client-core --example put_probe -- <base_url> <enrollment_key_a> <enrollment_key_b>
use tsute_client_core::secrets::FileSecretStore;
use tsute_client_core::{Client, Profile};
use tsute_proto::*;

#[tokio::main]
async fn main() {
    let a: Vec<String> = std::env::args().collect();
    let dir = tempfile_dir();
    let secrets = FileSecretStore { dir: dir.join("s") };
    let pa = Profile::new(&dir, "probe-a").unwrap();
    pa.enroll(&secrets, &a[1], &a[2], "E2E probe A").await.unwrap();
    let pb = Profile::new(&dir, "probe-b").unwrap();
    let cb = pb.enroll(&secrets, &a[1], &a[3], "E2E probe B").await.unwrap();
    let c = Client::open(pa, &secrets).unwrap();
    let api = c.api();
    for (name, size) in [("empty", 0u64), ("small", 12), ("two-chunk", 9 * 1024 * 1024)] {
        let t = api
            .create_transfer(&CreateTransferRequest {
                receiver: cb.endpoint_id.clone(),
                kind: TransferKind::Files,
                text: None,
                files: vec![
                    NewFile {
                        name: "pad".into(),
                        size: 1,
                        mime: "a/b".into(),
                        media: Default::default(),
                    },
                    NewFile {
                        name: name.into(),
                        size,
                        mime: "a/b".into(),
                        media: Default::default(),
                    },
                ],
                chunk_size: None,
            })
            .await
            .unwrap();
        for (file, f) in t.files.iter().enumerate() {
            for idx in 0..f.chunk_count {
                let len = t.chunk_len(file as u32, idx);
                let data = vec![7u8; len as usize];
                use base64::Engine;
                use sha2::Digest;
                let sha = base64::engine::general_purpose::STANDARD.encode(sha2::Sha256::digest(&data));
                let info = ChunkInfo {
                    file: file as u32,
                    index: idx,
                    size: len,
                    sha256: sha,
                };
                let u = api
                    .upload_urls(&t.transfer_id, vec![info])
                    .await
                    .unwrap()
                    .pop()
                    .unwrap();
                let r = api.put_blob(&u, data).await;
                println!(
                    "{name}: file={file} chunk={idx} len={len} headers={:?} -> {:?}",
                    u.headers.iter().map(|h| &h.0).collect::<Vec<_>>(),
                    r.as_ref()
                        .map_err(|e| e.to_string().chars().take(80).collect::<String>())
                );
            }
        }
        let _ = api.cancel(&t.transfer_id).await;
    }
}

fn tempfile_dir() -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("tsute-probe-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}
