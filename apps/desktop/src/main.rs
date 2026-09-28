//! つて デスクトップアプリ（macOS: メニューバー / Windows: 通知領域に常駐）
//!
//! 起動: `tsute [--profile NAME] [--show] [--insecure-file-credentials] [--data-dir DIR]`
//! - 常にウィンドウなしで起動し、メニューバー（通知領域）に常駐する（ログイン時の自動起動でもウィンドウを出さないため）。
//!   未登録のとき、または `--show` のときだけウィンドウを開く。
//! - ウィンドウの Close は WebView の破棄であり終了ではない。終了はメニューの「つて を終了」。

// Windows のリリースビルドでは GUI アプリとしてリンクし、起動時にコンソールウィンドウを出さない
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod automation;
mod commands;
mod state;
mod tray;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tauri::{AppHandle, Emitter, Manager, RunEvent, WebviewUrl, WebviewWindowBuilder};
use tsute_client_core::secrets::{FileSecretStore, SecretStore};
use tsute_client_core::{Client, ClientEvent, Direction, Profile};
use tsute_proto::TransferKind;

use crate::state::{AppState, Args};

#[cfg(not(windows))]
fn default_app_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join("Library/Application Support/dev.tsute.desktop")
}

/// %LOCALAPPDATA%（Roaming ではない）。移動プロファイルで他の PC に設定・DB・転送中データが複製されると、
/// 同じ Endpoint が 2 台で動いてしまうため
#[cfg(windows)]
fn default_app_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("dev.tsute.desktop")
}

pub fn show_window(app: &AppHandle, view: Option<&str>) {
    // ウィンドウを開いた = 受信を確認したとみなし、メニューバーの印を消す
    let had_unread = {
        let st = app.state::<AppState>();
        let mut u = st.unread.lock().expect("lock");
        let had = !u.is_empty();
        u.clear();
        had
    };
    if had_unread {
        tray::refresh(app);
    }
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
        if let Some(v) = view {
            let _ = w.emit("navigate", v);
        }
        return;
    }
    let st = app.state::<AppState>();
    let title = if st.args.profile == "default" {
        "つて".to_string()
    } else {
        format!("つて — {}", st.args.profile)
    };
    let url = match view {
        Some(v) => format!("index.html#{v}"),
        None => "index.html".into(),
    };
    match WebviewWindowBuilder::new(app, "main", WebviewUrl::App(url.into()))
        .title(title)
        .inner_size(440.0, 680.0)
        .min_inner_size(380.0, 480.0)
        .build()
    {
        Ok(w) => {
            let _ = w.set_focus();
        }
        Err(e) => tracing::error!(error = %e, "failed to open window"),
    }
}

pub fn start_client(app: &AppHandle) -> Result<(), tsute_client_core::Error> {
    let st = app.state::<AppState>();
    let client = Client::open(st.profile(), st.secrets.as_ref())?;
    if let Some(d) = &st.args.download_dir
        && client.config().download_dir.as_ref() != Some(d)
    {
        client.set_download_dir(Some(d.clone()))?;
    }
    *st.client.lock().expect("lock") = Some(client.clone());
    let app2 = app.clone();
    tauri::async_runtime::spawn(async move {
        let mut rx = client.subscribe();
        client.start();
        refresh_endpoints(&app2, &client).await;
        tray::refresh(&app2);
        loop {
            match rx.recv().await {
                Ok(ev) => on_client_event(&app2, &client, ev).await,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
    });
    Ok(())
}

async fn refresh_endpoints(app: &AppHandle, client: &Client) {
    if let Ok(eps) = client.api().endpoints().await {
        *app.state::<AppState>().endpoints.lock().expect("lock") = eps;
    }
}

fn human_size(n: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < units.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", units[i])
    }
}

async fn on_client_event(app: &AppHandle, client: &Client, ev: ClientEvent) {
    let _ = app.emit("client-event", &ev);
    match &ev {
        ClientEvent::Connection { .. } => tray::refresh(app),
        ClientEvent::EndpointsChanged => {
            refresh_endpoints(app, client).await;
            let _ = app.emit("endpoints-changed", ());
            tray::refresh(app);
        }
        ClientEvent::TransferUpdated { .. } => tray::refresh(app),
        ClientEvent::IncomingReady { transfer_id } => {
            let Ok(Some(item)) = client.item(transfer_id) else {
                return;
            };
            if item.direction != Direction::Incoming {
                return;
            }
            let st = app.state::<AppState>();
            // ウィンドウが前面で見えているときは既に確認できるので印を付けない
            let visible = app
                .get_webview_window("main")
                .is_some_and(|w| w.is_visible().unwrap_or(false) && w.is_focused().unwrap_or(false));
            if !visible {
                st.unread.lock().expect("lock").insert(transfer_id.clone());
            }
            if !st
                .endpoints
                .lock()
                .expect("lock")
                .iter()
                .any(|e| e.endpoint_id == item.transfer.sender)
            {
                refresh_endpoints(app, client).await;
            }
            let from = st.endpoint_name(&item.transfer.sender);
            let t = &item.transfer;
            // 通知には内容そのもの（テキスト本文や画像）を出さず、種類と大きさだけを示す（ロック画面等での露出を避ける）
            let body = match t.kind {
                TransferKind::ClipboardText => match &t.text {
                    Some(s) => format!("テキスト（{} 文字）", s.chars().count()),
                    None => format!("テキスト（{}）", human_size(t.total_bytes())),
                },
                TransferKind::ClipboardImage => {
                    let f = &t.files[0];
                    match (f.media.width, f.media.height) {
                        (Some(w), Some(h)) => format!("画像 {w}×{h}（{}）", human_size(f.size)),
                        _ => format!("画像（{}）", human_size(f.size)),
                    }
                }
                TransferKind::ClipboardVideo => format!("動画（{}）", human_size(t.total_bytes())),
                TransferKind::Files => format!("ファイル {} 件（{}）", t.files.len(), human_size(t.total_bytes())),
            };
            let (id, title) = (transfer_id.clone(), format!("{from} から受信しました"));
            let app2 = app.clone();
            let _ = app.run_on_main_thread(move || {
                let _ = &app2;
                tsute_os::notify(&id, &title, &body);
            });
            tray::refresh(app);
        }
        ClientEvent::TransferFailed { transfer_id, .. } => {
            let id = format!("failed-{transfer_id}");
            let _ = app.run_on_main_thread(move || {
                tsute_os::notify(&id, "つて: 転送に失敗しました", "詳細はアプリで確認してください")
            });
            tray::refresh(app);
        }
        _ => {}
    }
}

fn init_logging(profile_root: &std::path::Path) {
    // LaunchServices から起動すると stderr が見えないので、プロファイルごとのログファイルにも書く。
    // Clipboard 内容・トークン・presigned URL はログに出さない方針（各所で個別に配慮）。
    let dir = profile_root.join("logs");
    let _ = std::fs::create_dir_all(&dir);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("tsute.log"));
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "info,tsute_client_core=info,tsute_desktop=info".into());
    match file {
        Ok(f) => tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .with_writer(Mutex::new(f))
            .init(),
        Err(_) => tracing_subscriber::fmt().with_env_filter(filter).init(),
    }
}

/// 送信のために書き出した一時ファイルのうち古いものを掃除する（異常終了で残った分）
fn cleanup_outbox(profile: &Profile) {
    let Ok(rd) = std::fs::read_dir(profile.outbox_dir()) else {
        return;
    };
    let week = std::time::Duration::from_secs(8 * 24 * 3600);
    for e in rd.flatten() {
        if let Ok(m) = e.metadata()
            && m.modified()
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > week)
        {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

fn main() {
    let args = Args::parse();
    let app_dir = args.data_dir.clone().unwrap_or_else(default_app_dir);
    let profile = match Profile::new(&app_dir, &args.profile) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    init_logging(&profile.root);

    // 同一プロファイルの二重起動は WebSocket・ダウンロードが競合するので拒否する
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(profile.root.join("instance.lock"))
        .expect("lock file");
    if lock.try_lock().is_err() {
        eprintln!("profile {:?} is already running", args.profile);
        std::process::exit(0);
    }

    let secrets: Arc<dyn SecretStore> = if args.insecure_file_credentials {
        tracing::warn!("using INSECURE file credential store (development/testing only)");
        Arc::new(FileSecretStore {
            dir: profile.root.join("insecure-secrets"),
        })
    } else {
        #[cfg(target_os = "macos")]
        {
            Arc::new(tsute_client_core::secrets::KeychainSecretStore {
                service: "dev.tsute.desktop".into(),
            })
        }
        #[cfg(windows)]
        {
            Arc::new(tsute_client_core::secrets::WindowsCredentialStore {
                service: "dev.tsute.desktop".into(),
            })
        }
        // 対象外の OS（ビルド確認用）だけの開発用フォールバック。macOS / Windows の既定には使わない（ADR-0011）
        #[cfg(not(any(target_os = "macos", windows)))]
        {
            Arc::new(FileSecretStore {
                dir: profile.root.join("insecure-secrets"),
            })
        }
    };
    cleanup_outbox(&profile);
    let automation_socket = profile.root.join("automation.sock");
    let state = AppState {
        args: args.clone(),
        app_dir,
        secrets,
        client: Mutex::new(None),
        snapshot: Mutex::new(None),
        endpoints: Mutex::new(vec![]),
        unread: Mutex::new(Default::default()),
        _lock: lock,
    };

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            commands::get_state,
            commands::enroll,
            commands::list_endpoints,
            commands::read_clipboard,
            commands::cancel_clipboard,
            commands::send_clipboard,
            commands::prepare_files,
            commands::send_files,
            commands::history,
            commands::apply_to_clipboard,
            commands::reveal,
            commands::save_as,
            commands::cancel_transfer,
            commands::set_login_item,
            commands::rename_endpoint,
            commands::choose_download_dir,
            commands::forget_enrollment,
            commands::automation_result,
        ])
        .setup(move |app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);
            let handle = app.handle().clone();
            let h2 = handle.clone();
            tsute_os::init_notifications(move |id| {
                let target = id.strip_prefix("failed-").unwrap_or(&id).to_string();
                show_window(&h2, Some(&format!("focus:{target}")));
            });
            let enrolled = match start_client(&handle) {
                Ok(()) => true,
                Err(tsute_client_core::Error::NotEnrolled) => false,
                Err(e) => {
                    tracing::error!(error = %e, "failed to open profile");
                    false
                }
            };
            tray::create(&handle)?;
            if args.automation {
                automation::start(&handle, automation_socket.clone());
            }
            if !enrolled || args.show {
                show_window(&handle, None);
            }
            Ok(())
        })
        .on_window_event(|window, ev| {
            // Close = WebView を破棄してメモリを返す。バックグラウンド処理と常駐は継続する
            match ev {
                tauri::WindowEvent::CloseRequested { .. } => {
                    let _ = window.destroy();
                }
                // ウィンドウが前面に来た = 受信を確認できる状態なので、メニューバーの印を消す
                tauri::WindowEvent::Focused(true) => {
                    let app = window.app_handle();
                    let cleared = {
                        let st = app.state::<AppState>();
                        let mut u = st.unread.lock().expect("lock");
                        let had = !u.is_empty();
                        u.clear();
                        had
                    };
                    if cleared {
                        tray::refresh(app);
                    }
                }
                _ => {}
            }
        })
        .build(tauri::generate_context!())
        .expect("failed to build app");

    app.run(|app, ev| match ev {
        // 最後のウィンドウを閉じても終了しない（明示的な Quit / OS のログアウト時は code 付きまたは terminate で終了する）
        RunEvent::ExitRequested { code: None, api, .. } => api.prevent_exit(),
        #[cfg(target_os = "macos")]
        RunEvent::Reopen { .. } => show_window(app, None),
        RunEvent::Exit => {
            if let Some(c) = app.state::<AppState>().client() {
                c.shutdown();
            }
            let root = app.state::<AppState>().profile().root;
            if let Ok(p) = std::fs::read_to_string(root.join("automation.sock.path")) {
                let _ = std::fs::remove_file(p);
            }
            let _ = std::fs::remove_file(root.join("automation.sock"));
        }
        _ => {}
    });
}
