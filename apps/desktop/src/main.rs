// PoC: tray 常駐 / profile 引数 / window の破棄と再生成 / Accessory ポリシーを検証する
use tauri::{
    Manager, RunEvent, WebviewUrl, WebviewWindowBuilder,
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
};

fn profile_from_args() -> String {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == "--profile")
        .and_then(|i| args.get(i + 1).cloned())
        .unwrap_or_else(|| "default".into())
}

fn show_main(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.set_focus();
        return;
    }
    let profile = app.state::<String>().inner().clone();
    let _ = WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
        .title(format!("つて ({profile})"))
        .inner_size(480.0, 640.0)
        .build();
}

fn main() {
    let profile = profile_from_args();
    let app = tauri::Builder::default()
        .manage(profile.clone())
        .setup(move |app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);
            let open = MenuItem::with_id(app, "open", "Open つて", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open, &quit])?;
            let icon = tauri::image::Image::from_bytes(include_bytes!("../icons/tray.png"))?;
            let mut tb = TrayIconBuilder::with_id("main")
                .icon(icon)
                .icon_as_template(true)
                .tooltip(format!("つて — {}", profile))
                .menu(&menu)
                .on_menu_event(|app, ev| match ev.id().as_ref() {
                    "open" => show_main(app),
                    "quit" => app.exit(0),
                    _ => {}
                });
            if profile != "default" {
                tb = tb.title(profile.clone());
            }
            tb.build(app)?;
            Ok(())
        })
        .on_window_event(|window, ev| {
            // Close は終了ではなく WebView を破棄してアイドル時のメモリを解放する
            if let tauri::WindowEvent::CloseRequested { .. } = ev {
                let _ = window.destroy();
            }
        })
        .build(tauri::generate_context!())
        .expect("build app");
    app.run(|app, ev| match ev {
        RunEvent::ExitRequested { code: None, api, .. } => api.prevent_exit(),
        #[cfg(target_os = "macos")]
        RunEvent::Reopen { .. } => show_main(app),
        _ => {}
    });
}
