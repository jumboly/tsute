//! メニューバー（macOS）/ 通知領域（Windows）の常駐 UI。
//! 状態が変わるたびにメニューを作り直す（項目数が少なく、差分更新より単純で確実なため）。

use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager};
use tsute_client_core::Direction;
use tsute_client_core::ws::ConnState;
use tsute_proto::TransferKind;

use crate::state::AppState;

const TRAY_ID: &str = "main";

fn icon(online: bool, unread: bool) -> tauri::image::Image<'static> {
    // 未確認の受信を最優先で示す（ad-hoc 署名では OS 通知が使えないため、その代替。ADR-0014）
    let bytes: &'static [u8] = if unread {
        include_bytes!("../icons/tray-unread.png")
    } else if online {
        include_bytes!("../icons/tray.png")
    } else {
        include_bytes!("../icons/tray-offline.png")
    };
    tauri::image::Image::from_bytes(bytes).expect("tray icon")
}

pub fn create(app: &AppHandle) -> tauri::Result<()> {
    let st = app.state::<AppState>();
    let mut b = TrayIconBuilder::with_id(TRAY_ID)
        .icon(icon(false, false))
        .icon_as_template(true)
        .menu(&build_menu(app)?)
        .show_menu_on_left_click(true)
        .on_menu_event(on_menu);
    // 複数プロファイル同時起動時に区別できるよう、default 以外はアイコン横にプロファイル名を出す
    if st.args.profile != "default" {
        b = b.title(st.args.profile.clone());
    }
    b.build(app)?;
    refresh(app);
    Ok(())
}

fn kind_label(k: TransferKind) -> &'static str {
    match k {
        TransferKind::ClipboardText => "テキスト",
        TransferKind::ClipboardImage => "画像",
        TransferKind::ClipboardVideo => "動画",
        TransferKind::Files => "ファイル",
    }
}

fn build_menu(app: &AppHandle) -> tauri::Result<Menu<tauri::Wry>> {
    let st = app.state::<AppState>();
    let client = st.client();
    let status = match &client {
        None => "未登録 — Open から登録してください".to_string(),
        Some(c) => {
            let s = match c.connection_state() {
                ConnState::Online => "● 接続中",
                ConnState::Connecting => "○ 接続しています…",
                ConnState::Offline => "○ オフライン",
            };
            let unread = st.unread.lock().expect("lock").len();
            let mut line = format!("{s} — {} [{}]", c.config().name, st.args.profile);
            if unread > 0 {
                line.push_str(&format!(" · 未確認の受信 {unread} 件"));
            }
            line
        }
    };
    let status_item = MenuItem::with_id(app, "status", status, false, None::<&str>)?;
    let open = MenuItem::with_id(app, "open", "つて を開く", true, None::<&str>)?;
    let send_clip = MenuItem::with_id(
        app,
        "send_clipboard",
        "Clipboard を送る…",
        client.is_some(),
        None::<&str>,
    )?;
    let recent = Submenu::with_id(app, "recent", "最近の受信", true)?;
    let mut any = false;
    if let Some(c) = &client
        && let Ok(items) = c.history(30)
    {
        for it in items.iter().filter(|i| i.direction == Direction::Incoming).take(5) {
            let label = format!(
                "{} — {} ({:?})",
                kind_label(it.transfer.kind),
                st.endpoint_name(&it.transfer.sender),
                it.status
            );
            recent.append(&MenuItem::with_id(
                app,
                format!("item:{}", it.transfer.transfer_id),
                label,
                true,
                None::<&str>,
            )?)?;
            any = true;
        }
    }
    if !any {
        recent.append(&MenuItem::with_id(app, "recent_none", "（なし）", false, None::<&str>)?)?;
    }
    let login = tsute_os::login_item_status();
    let login_item = CheckMenuItem::with_id(
        app,
        "login_item",
        "ログイン時に起動",
        login != "unavailable",
        login == "enabled",
        None::<&str>,
    )?;
    let settings = MenuItem::with_id(app, "settings", "設定…", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "つて を終了", true, None::<&str>)?;
    let sep = || PredefinedMenuItem::separator(app);
    Menu::with_items(
        app,
        &[
            &status_item,
            &sep()?,
            &open,
            &send_clip,
            &recent,
            &sep()?,
            &login_item,
            &settings,
            &sep()?,
            &quit,
        ],
    )
}

fn on_menu(app: &AppHandle, ev: tauri::menu::MenuEvent) {
    let id = ev.id().as_ref().to_string();
    match id.as_str() {
        "open" => crate::show_window(app, None),
        "send_clipboard" => crate::show_window(app, Some("send_clipboard")),
        "settings" => crate::show_window(app, Some("settings")),
        "login_item" => {
            let enable = tsute_os::login_item_status() != "enabled";
            if let Err(e) = tsute_os::set_login_item(enable) {
                tracing::warn!(error = %e, "login item change failed");
            }
            refresh(app);
        }
        "quit" => app.exit(0),
        s if s.starts_with("item:") => crate::show_window(app, Some(&format!("focus:{}", &s[5..]))),
        _ => {}
    }
}

pub fn refresh(app: &AppHandle) {
    let app2 = app.clone();
    // メニュー操作はメインスレッドで行う
    let _ = app.run_on_main_thread(move || {
        let Some(tray) = app2.tray_by_id(TRAY_ID) else { return };
        let st = app2.state::<AppState>();
        let online = st.client().is_some_and(|c| c.connection_state() == ConnState::Online);
        let unread = st.unread.lock().expect("lock").len();
        let _ = tray.set_icon(Some(icon(online, unread > 0)));
        let _ = tray.set_icon_as_template(true);
        let mut tip = format!(
            "つて [{}] — {}",
            st.args.profile,
            if online { "接続中" } else { "オフライン" }
        );
        if unread > 0 {
            tip.push_str(&format!(" — 未確認の受信 {unread} 件"));
        }
        let _ = tray.set_tooltip(Some(tip));
        if let Ok(m) = build_menu(&app2) {
            let _ = tray.set_menu(Some(m));
        }
    });
}
