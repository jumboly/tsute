//! `tsute-devserver [--bind 127.0.0.1:8787] [--data-dir DIR]`
//!
//! 管理トークンは `<data-dir>/admin-token` に書き出す。ローカル専用のため
//! 管理経路を簡素にしているが、本番では IAM で保護された Lambda 直接呼び出しを使う（ADR-0002）。
use std::path::PathBuf;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let args: Vec<String> = std::env::args().collect();
    let arg = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned());
    let bind = arg("--bind").unwrap_or_else(|| "127.0.0.1:8787".into()).parse().expect("bind addr");
    let dir = PathBuf::from(arg("--data-dir").unwrap_or_else(|| "target/devserver".into()));
    std::fs::create_dir_all(&dir)?;
    let token = uuid::Uuid::new_v4().simple().to_string();
    std::fs::write(dir.join("admin-token"), &token)?;
    let s = tsute_server_local::start(bind, dir.join("blobs"), token, Default::default()).await?;
    tracing::info!(base_url = %s.base_url, "devserver listening");
    s.handle.await.ok();
    Ok(())
}
