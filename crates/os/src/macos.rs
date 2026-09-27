//! macOS 実装: NSPasteboard / AVFoundation / UserNotifications / ServiceManagement

#![allow(deprecated)] // AVAsset の同期 API（duration/tracks）は非推奨だが macOS 27 でも動作し、async 版より単純なため使う

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AnyThread, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSBitmapImageFileType, NSBitmapImageRep, NSImage, NSPasteboard, NSPasteboardTypePNG, NSPasteboardTypeString,
    NSPasteboardTypeTIFF, NSPasteboardWriting,
};
use objc2_foundation::{NSArray, NSBundle, NSData, NSDictionary, NSObject, NSObjectProtocol, NSString, NSURL};
use objc2_uniform_type_identifiers::{UTType, UTTypeImage, UTTypeMovie};

use crate::{ClipCandidate, ClipSnapshot, png_dimensions};

fn ns(s: &str) -> Retained<NSString> {
    NSString::from_str(s)
}

fn uttype_for_path(p: &Path) -> Option<Retained<UTType>> {
    let ext = p.extension()?.to_str()?;
    UTType::typeWithFilenameExtension(&ns(ext))
}

fn conforms(t: &UTType, to: &UTType) -> bool {
    t.conformsToType(to)
}

fn mime_of(t: &UTType, fallback: &str) -> String {
    t.preferredMIMEType().map(|m| m.to_string()).unwrap_or_else(|| fallback.to_string())
}

/// PNG 以外の画像データを PNG に変換する（受信側・貼り付け先の互換性を最大化するため PNG に正規化）
fn to_png(data: &NSData) -> Option<Vec<u8>> {
    let rep = NSBitmapImageRep::imageRepWithData(data)?;
    let props = NSDictionary::new();
    let png = unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &props) }?;
    Some(png.to_vec())
}

fn write_tmp(tmp: &Path, name: &str, data: &[u8]) -> Result<PathBuf, String> {
    std::fs::create_dir_all(tmp).map_err(|e| e.to_string())?;
    let p = tmp.join(name);
    std::fs::write(&p, data).map_err(|e| e.to_string())?;
    Ok(p)
}

fn stamp() -> String {
    let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    format!("{}{:03}", d.as_secs(), d.subsec_millis())
}

/// 動画の解像度と長さ。取れなければ None（表示用の補助情報なので失敗しても送信は妨げない）
pub fn video_info(path: &Path) -> (Option<u32>, Option<u32>, Option<u64>) {
    use objc2_av_foundation::{AVMediaTypeVideo, AVURLAsset};
    let Some(ps) = path.to_str() else { return (None, None, None) };
    let url = NSURL::fileURLWithPath(&ns(ps));
    let asset = unsafe { AVURLAsset::URLAssetWithURL_options(&url, None) };
    let dur = unsafe { asset.duration() };
    let duration_ms = (dur.timescale > 0 && dur.value >= 0).then(|| (dur.value as u64 * 1000) / dur.timescale as u64);
    let Some(media) = (unsafe { AVMediaTypeVideo }) else { return (None, None, duration_ms) };
    let tracks = unsafe { asset.tracksWithMediaType(media) };
    let size = tracks.firstObject().map(|t| unsafe { t.naturalSize() });
    match size {
        Some(s) if s.width > 0.0 => (Some(s.width.round() as u32), Some(s.height.round() as u32), duration_ms),
        _ => (None, None, duration_ms),
    }
}

fn candidate_for_file(p: &Path) -> Option<ClipCandidate> {
    let t = uttype_for_path(p)?;
    let meta = std::fs::metadata(p).ok()?;
    if !meta.is_file() {
        return None;
    }
    let name = p.file_name()?.to_string_lossy().to_string();
    if conforms(&t, unsafe { UTTypeMovie }) {
        let (width, height, duration_ms) = video_info(p);
        return Some(ClipCandidate::Video {
            path: p.to_path_buf(),
            name,
            mime: mime_of(&t, "video/quicktime"),
            size: meta.len(),
            width,
            height,
            duration_ms,
            source: "public.file-url".into(),
            temporary: false,
        });
    }
    if conforms(&t, unsafe { UTTypeImage }) {
        let rep = std::fs::read(p).ok().and_then(|b| NSBitmapImageRep::imageRepWithData(&NSData::with_bytes(&b)));
        let (width, height) = rep.map(|r| (Some(r.pixelsWide() as u32), Some(r.pixelsHigh() as u32))).unwrap_or((None, None));
        return Some(ClipCandidate::Image {
            path: p.to_path_buf(),
            name,
            mime: mime_of(&t, "application/octet-stream"),
            size: meta.len(),
            width,
            height,
            source: "public.file-url".into(),
            temporary: false,
        });
    }
    None
}

/// Send Clipboard 押下時にだけ呼ぶ。Clipboard の常時監視はしない（最重要原則）。
pub fn read_clipboard(tmp: &Path) -> Result<ClipSnapshot, String> {
    let pb = NSPasteboard::generalPasteboard();
    let types: Vec<String> = pb.types().map(|a| a.iter().map(|t| t.to_string()).collect()).unwrap_or_default();
    let mut candidates = Vec::new();

    // 1. file URL（Finder でのコピー）。同時に載るアイコンの TIFF は画像候補にしない
    let mut file_paths = Vec::new();
    if let Some(items) = pb.pasteboardItems() {
        for item in items.iter() {
            if let Some(s) = item.stringForType(&ns("public.file-url"))
                && let Some(url) = NSURL::URLWithString(&s)
                && let Some(path) = url.path()
            {
                file_paths.push(PathBuf::from(path.to_string()));
            }
        }
    }
    if !file_paths.is_empty() {
        match (file_paths.len(), candidate_for_file(&file_paths[0])) {
            (1, Some(c)) => candidates.push(c),
            _ => candidates.push(ClipCandidate::Files { paths: file_paths.clone() }),
        }
    } else {
        // 2. 動画の実データ（public.movie 準拠の型）
        for t in &types {
            let Some(ut) = UTType::typeWithIdentifier(&ns(t)) else { continue };
            if !conforms(&ut, unsafe { UTTypeMovie }) {
                continue;
            }
            if let Some(data) = pb.dataForType(&ns(t)) {
                let ext = ut.preferredFilenameExtension().map(|e| e.to_string()).unwrap_or_else(|| "mov".into());
                let bytes = data.to_vec();
                let name = format!("clipboard-{}.{ext}", stamp());
                let path = write_tmp(tmp, &name, &bytes)?;
                let (width, height, duration_ms) = video_info(&path);
                candidates.push(ClipCandidate::Video {
                    path,
                    name,
                    mime: mime_of(&ut, "video/quicktime"),
                    size: bytes.len() as u64,
                    width,
                    height,
                    duration_ms,
                    source: t.clone(),
                    temporary: true,
                });
                break;
            }
        }
    }

    // 3. テキスト
    if file_paths.is_empty()
        && let Some(s) = pb.stringForType(unsafe { NSPasteboardTypeString })
    {
        let text = s.to_string();
        if !text.is_empty() {
            candidates.push(ClipCandidate::Text { text });
        }
    }

    // 4. 画像データ（PNG 優先、それ以外は PNG に変換）
    if file_paths.is_empty() {
        let png_type = unsafe { NSPasteboardTypePNG };
        let (png, source) = if let Some(d) = pb.dataForType(png_type) {
            (Some(d.to_vec()), "public.png".to_string())
        } else if let Some(d) = pb.dataForType(unsafe { NSPasteboardTypeTIFF }) {
            (to_png(&d), "public.tiff".to_string())
        } else if types.iter().any(|t| {
            UTType::typeWithIdentifier(&ns(t)).is_some_and(|u| conforms(&u, unsafe { UTTypeImage }))
        }) {
            // JPEG/HEIC/PDF など: NSImage に読ませて TIFF 経由で PNG 化
            let img = NSImage::initWithPasteboard(NSImage::alloc(), &pb);
            let png = img.and_then(|i| i.TIFFRepresentation()).and_then(|t| to_png(&t));
            (png, "image (converted)".to_string())
        } else {
            (None, String::new())
        };
        if let Some(png) = png {
            let (w, h) = png_dimensions(&png).map(|(w, h)| (Some(w), Some(h))).unwrap_or((None, None));
            let name = format!("clipboard-{}.png", stamp());
            let path = write_tmp(tmp, &name, &png)?;
            candidates.push(ClipCandidate::Image {
                path,
                name,
                mime: "image/png".into(),
                size: png.len() as u64,
                width: w,
                height: h,
                source,
                temporary: true,
            });
        }
    }
    Ok(ClipSnapshot { candidates, types })
}

pub fn write_text(text: &str) -> Result<(), String> {
    let pb = NSPasteboard::generalPasteboard();
    pb.clearContents();
    if pb.setString_forType(&ns(text), unsafe { NSPasteboardTypeString }) {
        Ok(())
    } else {
        Err("failed to write text to clipboard".into())
    }
}

/// PNG と TIFF の両方を載せる（TIFF しか受け付けない古いアプリがあるため）
pub fn write_image_png(path: &Path) -> Result<(), String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let data = NSData::with_bytes(&bytes);
    let pb = NSPasteboard::generalPasteboard();
    pb.clearContents();
    let is_png = png_dimensions(&bytes).is_some();
    let tiff = NSBitmapImageRep::imageRepWithData(&data).and_then(|r| r.TIFFRepresentation());
    let mut ok = false;
    if is_png {
        ok |= pb.setData_forType(Some(&data), unsafe { NSPasteboardTypePNG });
    }
    if let Some(t) = tiff {
        ok |= pb.setData_forType(Some(&t), unsafe { NSPasteboardTypeTIFF });
    }
    if ok { Ok(()) } else { Err("failed to write image to clipboard".into()) }
}

/// 受信済みファイル（動画・複数ファイル）を file URL として載せる。Finder やメッセージ等へ貼り付けられる
pub fn write_file_urls(paths: &[PathBuf]) -> Result<(), String> {
    let urls: Vec<Retained<ProtocolObject<dyn NSPasteboardWriting>>> = paths
        .iter()
        .map(|p| ProtocolObject::from_retained(NSURL::fileURLWithPath(&ns(&p.to_string_lossy()))))
        .collect();
    let arr = NSArray::from_retained_slice(&urls);
    let pb = NSPasteboard::generalPasteboard();
    pb.clearContents();
    if pb.writeObjects(&arr) { Ok(()) } else { Err("failed to write file URLs to clipboard".into()) }
}

// ---------------- 通知 ----------------

/// .app バンドルとして起動しているか。UNUserNotificationCenter はバンドル外で呼ぶと例外になるため確認する
pub fn is_bundled_app() -> bool {
    let b = NSBundle::mainBundle();
    b.bundleIdentifier().is_some() && b.bundlePath().to_string().ends_with(".app")
}

static NOTIFY_CLICK: OnceLock<Box<dyn Fn(String) + Send + Sync>> = OnceLock::new();

mod delegate {
    use super::*;
    use block2::Block;
    use objc2_user_notifications::{
        UNNotification, UNNotificationPresentationOptions, UNNotificationResponse, UNUserNotificationCenter,
        UNUserNotificationCenterDelegate,
    };

    define_class!(
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "TsuteNotificationDelegate"]
        pub struct NotificationDelegate;

        unsafe impl NSObjectProtocol for NotificationDelegate {}

        unsafe impl UNUserNotificationCenterDelegate for NotificationDelegate {
            #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
            fn will_present(
                &self,
                _center: &UNUserNotificationCenter,
                _n: &UNNotification,
                handler: &Block<dyn Fn(UNNotificationPresentationOptions)>,
            ) {
                // メニューバー常駐アプリは前面扱いになり得るので、前面でもバナーを出す
                handler.call((UNNotificationPresentationOptions::Banner
                    | UNNotificationPresentationOptions::List
                    | UNNotificationPresentationOptions::Sound,));
            }

            #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
            fn did_receive(&self, _center: &UNUserNotificationCenter, resp: &UNNotificationResponse, handler: &Block<dyn Fn()>) {
                let id = resp.notification().request().identifier().to_string();
                if let Some(cb) = NOTIFY_CLICK.get() {
                    cb(id);
                }
                handler.call(());
            }
        }
    );

    impl NotificationDelegate {
        pub fn new(mtm: MainThreadMarker) -> Retained<Self> {
            let this = Self::alloc(mtm).set_ivars(());
            unsafe { msg_send![super(this), init] }
        }
    }
}

thread_local! {
    static DELEGATE: std::cell::RefCell<Option<Retained<delegate::NotificationDelegate>>> = const { std::cell::RefCell::new(None) };
}

/// メインスレッドで一度呼ぶ。通知クリック時に通知 ID（= transfer_id）で `on_click` が呼ばれる。
pub fn init_notifications(on_click: impl Fn(String) + Send + Sync + 'static) -> bool {
    use objc2_user_notifications::{UNAuthorizationOptions, UNUserNotificationCenter};
    if !is_bundled_app() {
        tracing::info!("not running as .app bundle; OS notifications disabled");
        return false;
    }
    let Some(mtm) = MainThreadMarker::new() else { return false };
    let _ = NOTIFY_CLICK.set(Box::new(on_click));
    let center = UNUserNotificationCenter::currentNotificationCenter();
    let d = delegate::NotificationDelegate::new(mtm);
    center.setDelegate(Some(ProtocolObject::from_ref(&*d)));
    DELEGATE.with(|cell| *cell.borrow_mut() = Some(d));
    let block = block2::RcBlock::new(|granted: objc2::runtime::Bool, _err: *mut objc2_foundation::NSError| {
        tracing::info!(granted = granted.as_bool(), "notification authorization");
    });
    center.requestAuthorizationWithOptions_completionHandler(
        UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound,
        &block,
    );
    true
}

pub fn notify(id: &str, title: &str, body: &str) {
    use objc2_user_notifications::{UNMutableNotificationContent, UNNotificationRequest, UNUserNotificationCenter};
    if !is_bundled_app() {
        tracing::info!(title, "notification (not bundled; skipped)");
        return;
    }
    let content = UNMutableNotificationContent::new();
    content.setTitle(&ns(title));
    content.setBody(&ns(body));
    let req = UNNotificationRequest::requestWithIdentifier_content_trigger(&ns(id), &content, None);
    UNUserNotificationCenter::currentNotificationCenter().addNotificationRequest_withCompletionHandler(&req, None);
}

// ---------------- ログイン時の自動起動 ----------------

/// SMAppService.mainApp（macOS 13+ の推奨 API）。システム設定 > 一般 > ログイン項目 に表示され、ユーザーがそこでも変更できる。
pub fn login_item_status() -> &'static str {
    use objc2_service_management::{SMAppService, SMAppServiceStatus};
    if !is_bundled_app() {
        return "unavailable";
    }
    let s = unsafe { SMAppService::mainAppService().status() };
    match s {
        SMAppServiceStatus::Enabled => "enabled",
        SMAppServiceStatus::RequiresApproval => "requires_approval",
        SMAppServiceStatus::NotRegistered => "not_registered",
        SMAppServiceStatus::NotFound => "not_found",
        _ => "unknown",
    }
}

pub fn set_login_item(enabled: bool) -> Result<(), String> {
    use objc2_service_management::SMAppService;
    if !is_bundled_app() {
        return Err("login item requires running as .app bundle".into());
    }
    let svc = unsafe { SMAppService::mainAppService() };
    let r = if enabled { unsafe { svc.registerAndReturnError() } } else { unsafe { svc.unregisterAndReturnError() } };
    r.map_err(|e| e.localizedDescription().to_string())
}

/// テスト専用: 任意の型で生データを Clipboard に載せる（動画の実データ表現などを再現するため）
#[doc(hidden)]
pub fn write_raw_for_test(uti: &str, bytes: &[u8]) -> Result<(), String> {
    let pb = NSPasteboard::generalPasteboard();
    pb.clearContents();
    if pb.setData_forType(Some(&NSData::with_bytes(bytes)), &ns(uti)) { Ok(()) } else { Err("write failed".into()) }
}

/// テスト専用: 現在の Clipboard のテキスト（退避・復元用）
#[doc(hidden)]
pub fn read_text_for_test() -> Option<String> {
    NSPasteboard::generalPasteboard().stringForType(unsafe { NSPasteboardTypeString }).map(|s| s.to_string())
}
