//! E2E テスト用のオートメーション経路（ADR-0013）。
//!
//! macOS の Accessibility 権限なしでも実際の UI を操作して E2E を回すため、
//! プロファイルディレクトリ内の Unix ドメインソケット（0600）/ Windows では名前付きパイプで JSON 行コマンドを受け付け、
//! WebView 内の DOM 操作（ボタンのクリック等）として実行する。UI → コマンド → Core → Cloud の経路は本番と同一。
//! `--automation` フラグと `TSUTE_AUTOMATION=1` の両方がないと起動しない。

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};
use tauri::{AppHandle, Manager};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::oneshot;

#[derive(Default)]
pub struct Pending {
    next: AtomicU64,
    waiters: Mutex<HashMap<u64, oneshot::Sender<(bool, Value)>>>,
}

pub fn complete(app: &AppHandle, id: u64, ok: bool, value: Value) {
    if let Some(p) = app.try_state::<Pending>()
        && let Some(tx) = p.waiters.lock().expect("lock").remove(&id)
    {
        let _ = tx.send((ok, value));
    }
}

async fn eval(app: &AppHandle, js: &str) -> Result<Value, String> {
    let w = app.get_webview_window("main").ok_or("window not open")?;
    let p = app.state::<Pending>();
    let id = p.next.fetch_add(1, Ordering::SeqCst);
    let (tx, rx) = oneshot::channel();
    p.waiters.lock().expect("lock").insert(id, tx);
    // JS 側の __tsuteAuto が結果を automation_result コマンドで返す
    // ページ読み込み前は __tsuteAuto が未定義なので、Tauri の IPC（初期化スクリプトで先に注入される）で即エラーを返す
    let wrapped = format!(
        "if (window.__tsuteAuto) {{ window.__tsuteAuto({id}, async () => {{ {js} }}); }} \
         else {{ window.__TAURI__.core.invoke('automation_result', {{ id: {id}, ok: false, value: 'ui not ready' }}); }}"
    );
    w.eval(&wrapped).map_err(|e| e.to_string())?;
    match tokio::time::timeout(std::time::Duration::from_secs(20), rx).await {
        Ok(Ok((true, v))) => Ok(v),
        Ok(Ok((false, v))) => Err(v.as_str().map(String::from).unwrap_or_else(|| v.to_string())),
        _ => {
            p.waiters.lock().expect("lock").remove(&id);
            Err("eval timeout".into())
        }
    }
}

async fn handle(app: &AppHandle, req: Value) -> Result<Value, String> {
    match req["cmd"].as_str().unwrap_or("") {
        "show" => {
            crate::show_window(app, req["view"].as_str());
            Ok(json!(true))
        }
        "hide" => {
            if let Some(w) = app.get_webview_window("main") {
                w.destroy().map_err(|e| e.to_string())?;
            }
            Ok(json!(true))
        }
        "window_open" => Ok(json!(app.get_webview_window("main").is_some())),
        "eval" => eval(app, req["js"].as_str().unwrap_or("")).await,
        "pid" => Ok(json!(std::process::id())),
        // メニューバーアイコンの「未確認の受信」印の状態
        "unread" => Ok(json!(
            app.state::<crate::state::AppState>().unread.lock().expect("lock").len()
        )),
        "quit" => {
            app.exit(0);
            Ok(json!(true))
        }
        c => Err(format!("unknown cmd {c:?}")),
    }
}

/// 1 接続分の JSON 行コマンドを処理する（Unix ソケットと名前付きパイプで共通）
async fn serve<S>(app: AppHandle, stream: S)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + 'static,
{
    let (r, mut w) = tokio::io::split(stream);
    let mut lines = BufReader::new(r).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let resp = match serde_json::from_str::<Value>(&line) {
            Ok(req) => match handle(&app, req).await {
                Ok(v) => json!({"ok": true, "value": v}),
                Err(e) => json!({"ok": false, "error": e}),
            },
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        };
        if w.write_all(format!("{resp}\n").as_bytes()).await.is_err() {
            break;
        }
    }
}

fn path_hash(p: &std::path::Path) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    p.hash(&mut h);
    h.finish()
}

/// Unix ソケットのパス長上限（macOS は 104 バイト）を超える場合は短い一時パスに逃がし、
/// 実際のパスを `<profile>/automation.sock.path` に書いてドライバが見つけられるようにする
#[cfg(unix)]
fn resolve_socket_path(preferred: &std::path::Path) -> std::path::PathBuf {
    if preferred.as_os_str().len() < 100 {
        return preferred.to_path_buf();
    }
    std::env::temp_dir().join(format!("tsute-auto-{:016x}.sock", path_hash(preferred)))
}

/// 名前付きパイプはファイルシステム上に置けないので、プロファイルのパスから名前を決める。
/// ドライバは `<profile>/automation.sock.path` から名前を読む（Unix と同じ手順）
#[cfg(windows)]
fn resolve_socket_path(preferred: &std::path::Path) -> std::path::PathBuf {
    std::path::PathBuf::from(format!(r"\\.\pipe\tsute-auto-{:016x}", path_hash(preferred)))
}

pub fn start(app: &AppHandle, preferred: std::path::PathBuf) {
    app.manage(Pending::default());
    let socket = resolve_socket_path(&preferred);
    let _ = std::fs::write(
        preferred.with_extension("sock.path"),
        socket.to_string_lossy().as_bytes(),
    );
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        #[cfg(unix)]
        {
            let _ = std::fs::remove_file(&socket);
            let listener = match tokio::net::UnixListener::bind(&socket) {
                Ok(l) => l,
                Err(e) => {
                    tracing::error!(error = %e, "automation socket bind failed");
                    return;
                }
            };
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600));
            tracing::warn!(socket = %socket.display(), "AUTOMATION ENABLED (test use only)");
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    continue;
                };
                tauri::async_runtime::spawn(serve(app.clone(), stream));
            }
        }
        #[cfg(windows)]
        {
            use tokio::net::windows::named_pipe::ServerOptions;
            // 既定のセキュリティ記述子では、書き込み（= コマンド送信）ができるのは作成したユーザーと
            // 管理者・SYSTEM だけ。リモートからの接続は拒否し、最初のインスタンスは他プロセスの先取りを防ぐ
            let mut first = true;
            tracing::warn!(pipe = %socket.display(), "AUTOMATION ENABLED (test use only)");
            loop {
                let server = match ServerOptions::new()
                    .first_pipe_instance(first)
                    .reject_remote_clients(true)
                    .create(&socket)
                {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::error!(error = %e, "automation pipe create failed");
                        return;
                    }
                };
                first = false;
                if server.connect().await.is_err() {
                    continue;
                }
                tauri::async_runtime::spawn(serve(app.clone(), server));
            }
        }
    });
}
