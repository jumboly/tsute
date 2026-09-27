//! UI（WebView）から呼ばれるコマンド。
//!
//! 最重要原則「ユーザーが明示的に送ると決めたデータだけを共有する」をここで守る:
//! - Clipboard を読むのは `read_clipboard`（Send Clipboard ボタン）だけ
//! - 転送を始めるのは `send_clipboard` / `send_files`（確認画面の Send ボタン）だけ
//! - OS Clipboard に書くのは `apply_to_clipboard`（受信項目のボタン）だけ

use std::path::{Path, PathBuf};

use base64::Engine;
use serde::Serialize;
use tauri::{AppHandle, State};
use tsute_client_core::{Direction, HistoryItem, LocalStatus, OutgoingFile};
use tsute_os::ClipCandidate;
use tsute_proto::{EndpointInfo, MediaInfo, TransferKind};

use crate::state::AppState;

type CmdResult<T> = Result<T, String>;

fn es(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// NSPasteboard / NSWorkspace 等をメインスレッドで実行する
pub async fn on_main<R: Send + 'static>(app: &AppHandle, f: impl FnOnce() -> R + Send + 'static) -> CmdResult<R> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.run_on_main_thread(move || {
        let _ = tx.send(f());
    })
    .map_err(es)?;
    rx.await.map_err(es)
}

#[derive(Serialize)]
pub struct UiState {
    profile: String,
    enrolled: bool,
    endpoint_id: Option<String>,
    endpoint_name: Option<String>,
    base_url: Option<String>,
    default_base_url: Option<String>,
    connection: String,
    credential_store: String,
    insecure_credentials: bool,
    download_dir: Option<PathBuf>,
    last_receiver: Option<String>,
    login_item: String,
    version: String,
}

#[tauri::command]
pub async fn get_state(app: AppHandle, st: State<'_, AppState>) -> CmdResult<UiState> {
    let c = st.client();
    let cfg = c.as_ref().map(|c| c.config());
    let login_item = on_main(&app, || tsute_os::login_item_status().to_string()).await?;
    Ok(UiState {
        profile: st.args.profile.clone(),
        enrolled: c.is_some(),
        endpoint_id: c.as_ref().map(|c| c.endpoint_id().to_string()),
        endpoint_name: cfg.as_ref().map(|c| c.name.clone()),
        base_url: cfg.as_ref().map(|c| c.base_url.clone()),
        // ビルド時に CI の Variables から注入（ソースに環境固有 URL を書かないため）
        default_base_url: option_env!("TSUTE_DEFAULT_BASE_URL").map(String::from),
        connection: c
            .as_ref()
            .map(|c| {
                serde_json::to_value(c.connection_state())
                    .ok()
                    .and_then(|v| v.as_str().map(String::from))
                    .unwrap_or_default()
            })
            .unwrap_or_else(|| "offline".into()),
        credential_store: st.secrets.describe(),
        insecure_credentials: st.args.insecure_file_credentials,
        download_dir: c.as_ref().map(|c| c.download_dir()),
        last_receiver: cfg.as_ref().and_then(|c| c.last_receiver.clone()),
        login_item,
        version: env!("CARGO_PKG_VERSION").into(),
    })
}

#[tauri::command]
pub async fn enroll(
    app: AppHandle,
    st: State<'_, AppState>,
    base_url: String,
    enrollment_key: String,
    name: String,
) -> CmdResult<()> {
    if st.client().is_some() {
        return Err("already enrolled".into());
    }
    let base = base_url.trim().trim_end_matches('/').to_string();
    if !(base.starts_with("https://") || base.starts_with("http://127.0.0.1") || base.starts_with("http://localhost")) {
        // 平文 HTTP はローカル開発サーバーだけに限る（トークンを平文で流さないため）
        return Err("base URL must start with https:// (http:// only for localhost)".into());
    }
    st.profile()
        .enroll(st.secrets.as_ref(), &base, &enrollment_key, name.trim())
        .await
        .map_err(es)?;
    crate::start_client(&app).map_err(es)?;
    Ok(())
}

#[tauri::command]
pub async fn list_endpoints(st: State<'_, AppState>) -> CmdResult<Vec<EndpointInfo>> {
    let c = st.require_client()?;
    let eps = c.api().endpoints().await.map_err(es)?;
    *st.endpoints.lock().expect("lock") = eps.clone();
    Ok(eps)
}

#[derive(Serialize)]
pub struct PreviewCandidate {
    index: usize,
    #[serde(flatten)]
    candidate: ClipCandidate,
    /// 画像・動画サムネイルの data URL（WebView に任意ファイルの読み取り権限を与えないため data URL で渡す）
    preview_data_url: Option<String>,
    char_count: Option<usize>,
    byte_size: u64,
}

#[derive(Serialize)]
pub struct Preview {
    candidates: Vec<PreviewCandidate>,
    types: Vec<String>,
}

fn data_url(mime: &str, bytes: &[u8]) -> String {
    format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    )
}

fn image_preview(path: &Path, mime: &str, size: u64) -> Option<String> {
    const INLINE_MAX: u64 = 8 * 1024 * 1024;
    let browser_ok = matches!(mime, "image/png" | "image/jpeg" | "image/gif" | "image/webp");
    if size <= INLINE_MAX && browser_ok {
        return std::fs::read(path).ok().map(|b| data_url(mime, &b));
    }
    // 大きい画像や HEIC/TIFF は macOS 標準の sips で縮小 PNG を作って表示する
    let out = std::env::temp_dir().join(format!("tsute-thumb-{}.png", std::process::id()));
    let ok = std::process::Command::new("sips")
        .args(["-s", "format", "png", "-Z", "800"])
        .arg(path)
        .arg("--out")
        .arg(&out)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    let r = ok
        .then(|| std::fs::read(&out).ok().map(|b| data_url("image/png", &b)))
        .flatten();
    let _ = std::fs::remove_file(out);
    r
}

/// Send Clipboard ボタン: この時点の Clipboard を 1 回だけ読み、プレビュー用に返す（まだ送らない）
#[tauri::command]
pub async fn read_clipboard(app: AppHandle, st: State<'_, AppState>) -> CmdResult<Preview> {
    let profile = st.profile();
    let outbox = profile.outbox_dir();
    discard_snapshot(&st);
    let snap = on_main(&app, move || tsute_os::read_clipboard(&outbox)).await??;
    let mut candidates = Vec::new();
    for (index, c) in snap.candidates.iter().enumerate() {
        let (preview_data_url, char_count, byte_size) = match c {
            ClipCandidate::Text { text } => (None, Some(text.chars().count()), text.len() as u64),
            ClipCandidate::Image { path, mime, size, .. } => (image_preview(path, mime, *size), None, *size),
            ClipCandidate::Video { path, size, .. } => {
                let p = path.clone();
                let thumb = on_main(&app, move || tsute_os::video_thumbnail_png(&p, 640.0)).await?;
                (thumb.map(|b| data_url("image/png", &b)), None, *size)
            }
            ClipCandidate::Files { paths } => (
                None,
                None,
                paths
                    .iter()
                    .filter_map(|p| std::fs::metadata(p).ok())
                    .map(|m| m.len())
                    .sum(),
            ),
        };
        candidates.push(PreviewCandidate {
            index,
            candidate: c.clone(),
            preview_data_url,
            char_count,
            byte_size,
        });
    }
    let types = snap.types.clone();
    *st.snapshot.lock().expect("lock") = Some(snap);
    Ok(Preview { candidates, types })
}

/// プレビューを閉じた（送らなかった）ときに、アプリが書き出した一時ファイルを消す
fn discard_snapshot(st: &AppState) {
    if let Some(snap) = st.snapshot.lock().expect("lock").take() {
        for c in snap.candidates {
            if let ClipCandidate::Image {
                path, temporary: true, ..
            }
            | ClipCandidate::Video {
                path, temporary: true, ..
            } = c
            {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

#[tauri::command]
pub async fn cancel_clipboard(st: State<'_, AppState>) -> CmdResult<()> {
    discard_snapshot(&st);
    Ok(())
}

/// 確認画面の Send ボタン: プレビューした内容（再読み取りしない）を送る
#[tauri::command]
pub async fn send_clipboard(st: State<'_, AppState>, index: usize, receiver: String) -> CmdResult<String> {
    let c = st.require_client()?;
    let snap = st
        .snapshot
        .lock()
        .expect("lock")
        .take()
        .ok_or("no clipboard preview; press Send Clipboard again")?;
    let cand = snap.candidates.get(index).cloned().ok_or("invalid candidate")?;
    // 選ばなかった候補の一時ファイルは不要
    for (i, other) in snap.candidates.iter().enumerate() {
        if i != index
            && let ClipCandidate::Image {
                path, temporary: true, ..
            }
            | ClipCandidate::Video {
                path, temporary: true, ..
            } = other
        {
            let _ = std::fs::remove_file(path);
        }
    }
    let t = match cand {
        ClipCandidate::Text { text } => c.send_text(&receiver, &text).await,
        ClipCandidate::Image {
            path,
            name,
            mime,
            width,
            height,
            ..
        } => {
            let f = OutgoingFile {
                path,
                name: Some(name),
                mime: Some(mime),
                media: MediaInfo {
                    width,
                    height,
                    duration_ms: None,
                },
            };
            c.send_files(&receiver, TransferKind::ClipboardImage, vec![f], None)
                .await
        }
        ClipCandidate::Video {
            path,
            name,
            mime,
            width,
            height,
            duration_ms,
            ..
        } => {
            let f = OutgoingFile {
                path,
                name: Some(name),
                mime: Some(mime),
                media: MediaInfo {
                    width,
                    height,
                    duration_ms,
                },
            };
            c.send_files(&receiver, TransferKind::ClipboardVideo, vec![f], None)
                .await
        }
        ClipCandidate::Files { paths } => {
            let files = paths
                .into_iter()
                .map(|p| OutgoingFile {
                    path: p,
                    name: None,
                    mime: None,
                    media: MediaInfo::default(),
                })
                .collect();
            c.send_files(&receiver, TransferKind::Files, files, None).await
        }
    }
    .map_err(es)?;
    let _ = c.remember_receiver(&receiver);
    Ok(t.transfer_id)
}

#[derive(Serialize)]
pub struct FileEntryUi {
    name: String,
    path: PathBuf,
    size: u64,
}

#[derive(Serialize)]
pub struct FilesPreview {
    files: Vec<FileEntryUi>,
    total_size: u64,
    /// 送信対象外（フォルダ等）とその理由
    rejected: Vec<(PathBuf, String)>,
}

/// Drop された時点では送らず、確認画面用の情報だけ返す
#[tauri::command]
pub async fn prepare_files(paths: Vec<PathBuf>) -> CmdResult<FilesPreview> {
    let mut files = Vec::new();
    let mut rejected = Vec::new();
    for p in paths {
        match std::fs::metadata(&p) {
            Ok(m) if m.is_file() => files.push(FileEntryUi {
                name: p
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default(),
                size: m.len(),
                path: p,
            }),
            Ok(m) if m.is_dir() => rejected.push((p, "フォルダは未対応です（zip にして送ってください）".into())),
            Ok(_) => rejected.push((p, "通常のファイルではありません".into())),
            Err(e) => rejected.push((p, e.to_string())),
        }
    }
    let total_size = files.iter().map(|f| f.size).sum();
    Ok(FilesPreview {
        files,
        total_size,
        rejected,
    })
}

#[tauri::command]
pub async fn send_files(st: State<'_, AppState>, paths: Vec<PathBuf>, receiver: String) -> CmdResult<String> {
    let c = st.require_client()?;
    let files = paths
        .into_iter()
        .map(|p| OutgoingFile {
            path: p,
            name: None,
            mime: None,
            media: MediaInfo::default(),
        })
        .collect();
    let t = c
        .send_files(&receiver, TransferKind::Files, files, None)
        .await
        .map_err(es)?;
    let _ = c.remember_receiver(&receiver);
    Ok(t.transfer_id)
}

#[derive(Serialize)]
pub struct HistoryUi {
    #[serde(flatten)]
    item: HistoryItem,
    peer_name: String,
    progress: Option<(u64, u64)>,
    text_preview: Option<String>,
    files_exist: bool,
}

#[tauri::command]
pub async fn history(st: State<'_, AppState>) -> CmdResult<Vec<HistoryUi>> {
    let c = st.require_client()?;
    let items = c.history(50).map_err(es)?;
    Ok(items
        .into_iter()
        .map(|item| {
            let peer = if item.direction == Direction::Incoming {
                &item.transfer.sender
            } else {
                &item.transfer.receiver
            };
            let text_preview = if item.transfer.kind == TransferKind::ClipboardText {
                match &item.transfer.text {
                    Some(t) => Some(t.chars().take(300).collect()),
                    None if item.direction == Direction::Incoming && item.status == LocalStatus::Done => item
                        .paths
                        .first()
                        .and_then(|p| std::fs::read_to_string(p).ok())
                        .map(|t| t.chars().take(300).collect()),
                    None => None,
                }
            } else {
                None
            };
            HistoryUi {
                peer_name: st.endpoint_name(peer),
                progress: c.progress(&item.transfer.transfer_id),
                files_exist: !item.paths.is_empty() && item.paths.iter().all(|p| p.exists()),
                text_preview,
                item,
            }
        })
        .collect())
}

fn done_incoming(st: &AppState, id: &str) -> CmdResult<HistoryItem> {
    let c = st.require_client()?;
    let item = c.item(id).map_err(es)?.ok_or("unknown transfer")?;
    if item.direction != Direction::Incoming || item.status != LocalStatus::Done {
        return Err("transfer is not received yet".into());
    }
    Ok(item)
}

/// 受信項目のボタン: ユーザー操作でだけ OS Clipboard に反映する
#[tauri::command]
pub async fn apply_to_clipboard(app: AppHandle, st: State<'_, AppState>, transfer_id: String) -> CmdResult<()> {
    let item = done_incoming(&st, &transfer_id)?;
    let t = item.transfer;
    let paths = item.paths;
    match t.kind {
        TransferKind::ClipboardText => {
            let text = match t.text {
                Some(s) => s,
                None => std::fs::read_to_string(paths.first().ok_or("missing file")?).map_err(es)?,
            };
            on_main(&app, move || tsute_os::write_text(&text)).await?
        }
        TransferKind::ClipboardImage => {
            let p = paths.first().cloned().ok_or("missing file")?;
            on_main(&app, move || tsute_os::write_image_png(&p)).await?
        }
        TransferKind::ClipboardVideo | TransferKind::Files => {
            on_main(&app, move || tsute_os::write_file_urls(&paths)).await?
        }
    }
}

#[tauri::command]
pub async fn reveal(app: AppHandle, st: State<'_, AppState>, transfer_id: String) -> CmdResult<()> {
    let c = st.require_client()?;
    let item = c.item(&transfer_id).map_err(es)?.ok_or("unknown transfer")?;
    let paths: Vec<PathBuf> = item.paths.into_iter().filter(|p| p.exists()).collect();
    if paths.is_empty() {
        return Err("file not found".into());
    }
    on_main(&app, move || tsute_os::reveal_in_finder(&paths)).await
}

/// 受信した画像・動画・テキストを任意の場所に保存する
#[tauri::command]
pub async fn save_as(app: AppHandle, st: State<'_, AppState>, transfer_id: String) -> CmdResult<Option<PathBuf>> {
    use tauri_plugin_dialog::DialogExt;
    let item = done_incoming(&st, &transfer_id)?;
    let (name, src): (String, Option<PathBuf>) = match (&item.transfer.text, item.paths.first()) {
        (Some(_), _) => ("clipboard.txt".into(), None),
        (None, Some(p)) => (
            item.transfer.files.first().map(|f| f.name.clone()).unwrap_or_default(),
            Some(p.clone()),
        ),
        _ => return Err("nothing to save".into()),
    };
    let Some(dest) = app.dialog().file().set_file_name(&name).blocking_save_file() else {
        return Ok(None);
    };
    let dest = dest.into_path().map_err(es)?;
    match src {
        Some(s) => std::fs::copy(&s, &dest).map(|_| ()).map_err(es)?,
        None => std::fs::write(&dest, item.transfer.text.unwrap_or_default()).map_err(es)?,
    }
    Ok(Some(dest))
}

#[tauri::command]
pub async fn cancel_transfer(st: State<'_, AppState>, transfer_id: String) -> CmdResult<()> {
    st.require_client()?.cancel(&transfer_id).await.map_err(es)
}

#[tauri::command]
pub async fn set_login_item(app: AppHandle, enabled: bool) -> CmdResult<String> {
    on_main(&app, move || tsute_os::set_login_item(enabled)).await??;
    let s = on_main(&app, || tsute_os::login_item_status().to_string()).await?;
    crate::tray::refresh(&app);
    Ok(s)
}

#[tauri::command]
pub async fn rename_endpoint(st: State<'_, AppState>, name: String) -> CmdResult<()> {
    st.require_client()?.rename(name.trim()).await.map_err(es)
}

#[tauri::command]
pub async fn choose_download_dir(app: AppHandle, st: State<'_, AppState>) -> CmdResult<Option<PathBuf>> {
    use tauri_plugin_dialog::DialogExt;
    let c = st.require_client()?;
    let Some(dir) = app.dialog().file().blocking_pick_folder() else {
        return Ok(None);
    };
    let dir = dir.into_path().map_err(es)?;
    c.set_download_dir(Some(dir.clone())).map_err(es)?;
    Ok(Some(dir))
}

/// ローカルの登録情報を削除（サーバー側の失効は管理者が scripts/admin.sh で行う）
#[tauri::command]
pub async fn forget_enrollment(app: AppHandle, st: State<'_, AppState>) -> CmdResult<()> {
    if let Some(c) = st.client.lock().expect("lock").take() {
        c.shutdown();
    }
    st.profile().forget(st.secrets.as_ref()).map_err(es)?;
    crate::tray::refresh(&app);
    Ok(())
}

#[tauri::command]
pub async fn automation_result(app: AppHandle, id: u64, ok: bool, value: serde_json::Value) -> CmdResult<()> {
    crate::automation::complete(&app, id, ok, value);
    Ok(())
}
