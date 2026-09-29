//! ローカル開発サーバーを相手にした結合テスト。
//!
//! モックではなく実際の HTTP / WebSocket / presigned URL 転送 / ファイル I/O を通す。
//! 「プロセス強制終了」は Client を専用 tokio runtime で動かし、runtime ごと破棄して再現する
//! （実行中タスクが後始末なしに止まる点がプロセス kill と同じ）。

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::Duration;

use tokio::sync::broadcast;
use tsute_client_core::secrets::FileSecretStore;
use tsute_client_core::*;
use tsute_proto::*;

struct Env {
    server: tsute_server_local::Server,
    tmp: tempfile::TempDir,
}

async fn env() -> Env {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("warn,tsute=debug")
        .with_test_writer()
        .try_init();
    let tmp = tempfile::tempdir().unwrap();
    let server = tsute_server_local::start(
        "127.0.0.1:0".parse().unwrap(),
        tmp.path().join("blobs"),
        "admin".into(),
        Default::default(),
    )
    .await
    .unwrap();
    Env { server, tmp }
}

impl Env {
    fn app_dir(&self) -> PathBuf {
        self.tmp.path().join("app")
    }
    fn secrets(&self) -> FileSecretStore {
        FileSecretStore {
            dir: self.tmp.path().join("secrets"),
        }
    }
    async fn enroll(&self, profile: &str) -> Client {
        let p = Profile::new(&self.app_dir(), profile).unwrap();
        let key = self.server.issue_enrollment_key().await;
        let mut cfg = p
            .enroll(&self.secrets(), &self.server.base_url, &key, profile)
            .await
            .unwrap();
        cfg.download_dir = Some(self.tmp.path().join(format!("dl-{profile}")));
        p.save_config(&cfg).unwrap();
        self.open(profile)
    }
    fn open(&self, profile: &str) -> Client {
        Client::open(Profile::new(&self.app_dir(), profile).unwrap(), &self.secrets()).unwrap()
    }
}

/// 別 runtime（= 別プロセス相当）で Client を動かす
struct Proc {
    rt: Option<tokio::runtime::Runtime>,
}

impl Proc {
    fn spawn(client: Client) -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let c = client.clone();
        rt.spawn(async move { c.start() });
        Self { rt: Some(rt) }
    }
    fn kill(mut self) {
        self.rt.take().unwrap().shutdown_background();
    }
    fn block<F: std::future::Future + Send + 'static>(&self, f: F) -> F::Output
    where
        F::Output: Send + 'static,
    {
        let h = self.rt.as_ref().unwrap().spawn(f);
        futures_block(h)
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
    }
}

fn futures_block<T: Send + 'static>(h: tokio::task::JoinHandle<T>) -> T {
    // テスト本体の runtime とは別スレッドで待つ（runtime のネストを避ける）
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(h)
            .unwrap()
    })
    .join()
    .unwrap()
}

async fn wait_for<F: FnMut(&ClientEvent) -> bool>(
    rx: &mut broadcast::Receiver<ClientEvent>,
    secs: u64,
    mut f: F,
) -> ClientEvent {
    tokio::time::timeout(Duration::from_secs(secs), async {
        loop {
            match rx.recv().await {
                Ok(e) if f(&e) => return e,
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(e) => panic!("channel closed: {e}"),
            }
        }
    })
    .await
    .expect("timed out waiting for event")
}

async fn wait_ready(rx: &mut broadcast::Receiver<ClientEvent>, id: &str, secs: u64) {
    wait_for(
        rx,
        secs,
        |e| matches!(e, ClientEvent::IncomingReady { transfer_id } if transfer_id == id),
    )
    .await;
}

async fn wait_online(rx: &mut broadcast::Receiver<ClientEvent>) {
    wait_for(rx, 10, |e| {
        matches!(
            e,
            ClientEvent::Connection {
                state: tsute_client_core::ws::ConnState::Online
            }
        )
    })
    .await;
}

fn make_file(dir: &Path, name: &str, size: usize, seed: u8) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let p = dir.join(name);
    // 位置ごとに異なる内容にして、チャンクの取り違え（offset 誤り）を検出できるようにする
    let data: Vec<u8> = (0..size)
        .map(|i| (((i as u64 * 2654435761) >> 13) as u8) ^ seed)
        .collect();
    std::fs::write(&p, data).unwrap();
    p
}

fn outfile(p: &Path) -> OutgoingFile {
    OutgoingFile {
        path: p.to_path_buf(),
        name: None,
        mime: None,
        media: Default::default(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn enrollment_key_is_single_use_and_endpoints_listed() {
    let env = env().await;
    let key = env.server.issue_enrollment_key().await;
    let p = Profile::new(&env.app_dir(), "a").unwrap();
    p.enroll(&env.secrets(), &env.server.base_url, &key, "A").await.unwrap();
    let p2 = Profile::new(&env.app_dir(), "b").unwrap();
    let err = p2
        .enroll(&env.secrets(), &env.server.base_url, &key, "B")
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Api { status: 403, .. }), "{err}");
    let err = p2
        .enroll(&env.secrets(), &env.server.base_url, "tsute-ek-bogus", "B")
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Api { status: 403, .. }));

    let a = env.open("a");
    let b = env.enroll("b").await;
    let mut ra = a.subscribe();
    a.start();
    wait_online(&mut ra).await;
    let mut rb = b.subscribe();
    b.start();
    wait_online(&mut rb).await;
    // B の接続で A に presence 通知が届く
    wait_for(&mut ra, 10, |e| matches!(e, ClientEvent::EndpointsChanged)).await;
    let eps = a.api().endpoints().await.unwrap();
    assert_eq!(eps.len(), 2);
    assert!(eps.iter().all(|e| e.online), "{eps:?}");
    b.shutdown();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let eps = a.api().endpoints().await.unwrap();
    assert!(!eps.iter().find(|e| e.endpoint_id == b.endpoint_id()).unwrap().online);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn text_inline_and_large_text() {
    let env = env().await;
    let a = env.enroll("a").await;
    let b = env.enroll("b").await;
    let mut rb = b.subscribe();
    a.start();
    b.start();
    wait_online(&mut rb).await;

    let t = a.send_text(b.endpoint_id(), "こんにちは 🌏").await.unwrap();
    wait_ready(&mut rb, &t.transfer_id, 10).await;
    let item = b.item(&t.transfer_id).unwrap().unwrap();
    assert_eq!(item.transfer.text.as_deref(), Some("こんにちは 🌏"));

    let big = "あ".repeat(40_000); // 120KB > inline 上限
    let t = a.send_text(b.endpoint_id(), &big).await.unwrap();
    assert!(t.text.is_none());
    wait_ready(&mut rb, &t.transfer_id, 20).await;
    let item = b.item(&t.transfer_id).unwrap().unwrap();
    assert_eq!(std::fs::read_to_string(&item.paths[0]).unwrap(), big);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn single_multiple_and_large_files() {
    let env = env().await;
    let a = env.enroll("a").await;
    let b = env.enroll("b").await;
    let (mut ra, mut rb) = (a.subscribe(), b.subscribe());
    a.start();
    b.start();
    wait_online(&mut rb).await;
    let src = env.tmp.path().join("src");

    // 単一の小さいファイル + 空ファイル + 同名衝突
    let small = make_file(&src, "hello.bin", 1234, 1);
    let empty = make_file(&src, "empty.dat", 0, 0);
    let t = a
        .send_files(
            b.endpoint_id(),
            TransferKind::Files,
            vec![outfile(&small), outfile(&empty)],
            None,
        )
        .await
        .unwrap();
    wait_ready(&mut rb, &t.transfer_id, 20).await;
    let item = b.item(&t.transfer_id).unwrap().unwrap();
    assert_eq!(std::fs::read(&item.paths[0]).unwrap(), std::fs::read(&small).unwrap());
    assert_eq!(std::fs::metadata(&item.paths[1]).unwrap().len(), 0);
    wait_for(
        &mut ra,
        10,
        |e| matches!(e, ClientEvent::Delivered { transfer_id } if *transfer_id == t.transfer_id),
    )
    .await;
    assert_eq!(a.item(&t.transfer_id).unwrap().unwrap().status, LocalStatus::Done);

    let t2 = a
        .send_files(b.endpoint_id(), TransferKind::Files, vec![outfile(&small)], None)
        .await
        .unwrap();
    wait_ready(&mut rb, &t2.transfer_id, 20).await;
    let item2 = b.item(&t2.transfer_id).unwrap().unwrap();
    assert!(item2.paths[0].ends_with("hello (1).bin"), "{:?}", item2.paths);

    // 大きいファイル（多数チャンク, 端数あり）
    let big = make_file(&src, "big.bin", 9 * 1024 * 1024 + 777, 7);
    let t3 = a
        .send_files(
            b.endpoint_id(),
            TransferKind::Files,
            vec![outfile(&big)],
            Some(tsute_proto::MIN_CHUNK_SIZE),
        )
        .await
        .unwrap();
    assert_eq!(t3.files[0].chunk_count, 37);
    wait_ready(&mut rb, &t3.transfer_id, 60).await;
    let item3 = b.item(&t3.transfer_id).unwrap().unwrap();
    assert_eq!(std::fs::read(&item3.paths[0]).unwrap(), std::fs::read(&big).unwrap());
    // 受領後はサーバー上の一時データが消える
    assert!(!env.tmp.path().join("blobs/transfers").join(&t3.transfer_id).exists());
    // part ファイルが残っていない
    let leftovers: Vec<_> = std::fs::read_dir(item3.paths[0].parent().unwrap())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".tsute-part"))
        .collect();
    assert!(leftovers.is_empty());
    // Windows ではブラウザのダウンロードと同じ Mark of the Web が付く
    #[cfg(windows)]
    {
        let mut ads = item3.paths[0].as_os_str().to_owned();
        ads.push(":Zone.Identifier");
        let zone = std::fs::read_to_string(&ads).expect("Zone.Identifier");
        assert!(zone.contains("ZoneId=3"), "{zone}");
    }
}

/// 送信側を途中で kill → 受信側は既にアップロード済みチャンクを取得（overlap）→ 送信側再起動で再開して完了
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn upload_overlap_and_resume_after_sender_restart() {
    let env = env().await;
    let a = env.enroll("a").await;
    let b = env.enroll("b").await;
    let mut rb = b.subscribe();
    b.start();
    wait_online(&mut rb).await;
    let src = env.tmp.path().join("src");
    let big = make_file(&src, "resume.bin", 16 * 1024 * 1024, 3);
    let a_proc = Proc::spawn(a.clone());
    let b_id = b.endpoint_id().to_string();
    let a2 = a.clone();
    let big2 = big.clone();
    let t = a_proc.block(async move {
        a2.send_files(
            &b_id,
            TransferKind::Files,
            vec![outfile(&big2)],
            Some(tsute_proto::MIN_CHUNK_SIZE),
        )
        .await
        .unwrap()
    });
    // 受信側がダウンロードを始めた（= 送信完了前に一部チャンクが取得された）ところで送信側を kill
    wait_for(&mut rb, 30, |e| {
        matches!(e, ClientEvent::Progress { transfer_id, direction: Direction::Incoming, done_bytes, .. }
            if *transfer_id == t.transfer_id && *done_bytes > 0)
    })
    .await;
    a_proc.kill();
    let detail = b.api().transfer(&t.transfer_id).await.unwrap();
    assert_eq!(
        detail.transfer.state,
        TransferState::Uploading,
        "sender must be killed before finishing"
    );
    let uploaded = detail.chunks.len();
    assert!(uploaded > 0 && uploaded < 64, "uploaded={uploaded}");
    tokio::time::sleep(Duration::from_millis(500)).await;
    let local_got = b.progress(&t.transfer_id).unwrap().0;
    assert!(
        local_got > 0,
        "receiver should have downloaded some chunks while sender was still uploading"
    );

    // 「アプリ再起動」: 同じプロファイルを開き直すと残りだけアップロードされる
    let a_restarted = env.open("a");
    a_restarted.start();
    wait_ready(&mut rb, &t.transfer_id, 60).await;
    let item = b.item(&t.transfer_id).unwrap().unwrap();
    assert_eq!(std::fs::read(&item.paths[0]).unwrap(), std::fs::read(&big).unwrap());
}

/// 受信側を途中で kill → 再起動後、ローカルに書き込み済みのチャンクは取り直さずに完了
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn download_resume_after_receiver_restart() {
    let env = env().await;
    let a = env.enroll("a").await;
    let b = env.enroll("b").await;
    let src = env.tmp.path().join("src");
    let big = make_file(&src, "dl.bin", 12 * 1024 * 1024, 9);
    a.start();
    // 先に全部アップロードしておく（受信側オフライン中の保持も兼ねて確認）
    let mut ra = a.subscribe();
    let t = a
        .send_files(
            b.endpoint_id(),
            TransferKind::Files,
            vec![outfile(&big)],
            Some(tsute_proto::MIN_CHUNK_SIZE),
        )
        .await
        .unwrap();
    wait_for(&mut ra, 30, |e| {
        matches!(e, ClientEvent::TransferUpdated { transfer_id } if *transfer_id == t.transfer_id
        && a.item(transfer_id).unwrap().unwrap().status == LocalStatus::Uploaded)
    })
    .await;

    let b_proc = Proc::spawn(b.clone());
    let mut rb = b.subscribe();
    wait_for(&mut rb, 30, |e| {
        matches!(e, ClientEvent::Progress { transfer_id, direction: Direction::Incoming, done_bytes, .. }
            if *transfer_id == t.transfer_id && *done_bytes >= 3 * 1024 * 1024)
    })
    .await;
    b_proc.kill();
    tokio::time::sleep(Duration::from_millis(300)).await;

    let b2 = env.open("b");
    let mut rb2 = b2.subscribe();
    b2.start();
    // 再開直後の進捗が 0 でない = 既に受信済みのチャンクを再利用している
    let first = wait_for(
        &mut rb2,
        30,
        |e| matches!(e, ClientEvent::Progress { transfer_id, .. } if *transfer_id == t.transfer_id),
    )
    .await;
    if let ClientEvent::Progress { done_bytes, .. } = first {
        assert!(
            done_bytes >= 3 * 1024 * 1024,
            "resume should keep downloaded chunks, got {done_bytes}"
        );
    }
    wait_ready(&mut rb2, &t.transfer_id, 60).await;
    let item = b2.item(&t.transfer_id).unwrap().unwrap();
    assert_eq!(std::fs::read(&item.paths[0]).unwrap(), std::fs::read(&big).unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn corrupted_chunk_is_detected() {
    let env = env().await;
    let a = env.enroll("a").await;
    let b = env.enroll("b").await;
    let src = env.tmp.path().join("src");
    let f = make_file(&src, "c.bin", 1024 * 1024, 5);
    a.start();
    let mut ra = a.subscribe();
    let t = a
        .send_files(
            b.endpoint_id(),
            TransferKind::Files,
            vec![outfile(&f)],
            Some(tsute_proto::MIN_CHUNK_SIZE),
        )
        .await
        .unwrap();
    wait_for(&mut ra, 30, |e| {
        matches!(e, ClientEvent::TransferUpdated { transfer_id } if *transfer_id == t.transfer_id
        && a.item(transfer_id).unwrap().unwrap().status == LocalStatus::Uploaded)
    })
    .await;
    // Object Storage 上のチャンクを改ざん（ビット反転）
    let blob = env
        .tmp
        .path()
        .join("blobs")
        .join(tsute_server_core::blob_key(&t.transfer_id, 0, 2));
    let mut data = std::fs::read(&blob).unwrap();
    data[100] ^= 0xff;
    std::fs::write(&blob, data).unwrap();

    let mut rb = b.subscribe();
    b.start();
    let e = wait_for(
        &mut rb,
        30,
        |e| matches!(e, ClientEvent::TransferFailed { transfer_id, .. } if *transfer_id == t.transfer_id),
    )
    .await;
    assert!(format!("{e:?}").contains("checksum"), "{e:?}");
    // 壊れたデータで最終ファイルが作られていない
    let item = b.item(&t.transfer_id).unwrap().unwrap();
    assert!(!item.paths[0].exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transient_storage_outage_is_retried() {
    let env = env().await;
    let a = env.enroll("a").await;
    let b = env.enroll("b").await;
    let mut rb = b.subscribe();
    a.start();
    b.start();
    wait_online(&mut rb).await;
    let src = env.tmp.path().join("src");
    let f = make_file(&src, "flaky.bin", 3 * 1024 * 1024, 11);
    env.server.state.fail_blobs.store(true, Ordering::SeqCst);
    let t = a
        .send_files(
            b.endpoint_id(),
            TransferKind::Files,
            vec![outfile(&f)],
            Some(tsute_proto::MIN_CHUNK_SIZE),
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    env.server.state.fail_blobs.store(false, Ordering::SeqCst);
    wait_ready(&mut rb, &t.transfer_id, 60).await;
    let item = b.item(&t.transfer_id).unwrap().unwrap();
    assert_eq!(std::fs::read(&item.paths[0]).unwrap(), std::fs::read(&f).unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn websocket_reconnects_after_server_kick() {
    let env = env().await;
    let a = env.enroll("a").await;
    let b = env.enroll("b").await;
    let mut rb = b.subscribe();
    a.start();
    b.start();
    wait_online(&mut rb).await;
    tsute_server_local::kick_all(&env.server.state);
    wait_for(&mut rb, 10, |e| {
        matches!(
            e,
            ClientEvent::Connection {
                state: tsute_client_core::ws::ConnState::Offline
            }
        )
    })
    .await;
    wait_online(&mut rb).await;
    let t = a.send_text(b.endpoint_id(), "after reconnect").await.unwrap();
    wait_ready(&mut rb, &t.transfer_id, 10).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_cleans_up() {
    let env = env().await;
    let a = env.enroll("a").await;
    let b = env.enroll("b").await;
    a.start();
    let src = env.tmp.path().join("src");
    let f = make_file(&src, "x.bin", 2 * 1024 * 1024, 2);
    let mut ra = a.subscribe();
    let t = a
        .send_files(
            b.endpoint_id(),
            TransferKind::Files,
            vec![outfile(&f)],
            Some(tsute_proto::MIN_CHUNK_SIZE),
        )
        .await
        .unwrap();
    wait_for(&mut ra, 30, |e| {
        matches!(e, ClientEvent::TransferUpdated { transfer_id } if *transfer_id == t.transfer_id
        && a.item(transfer_id).unwrap().unwrap().status == LocalStatus::Uploaded)
    })
    .await;
    a.cancel(&t.transfer_id).await.unwrap();
    assert!(!env.tmp.path().join("blobs/transfers").join(&t.transfer_id).exists());
    let mut rb = b.subscribe();
    b.start();
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(
        b.item(&t.transfer_id).unwrap().is_none()
            || b.item(&t.transfer_id).unwrap().unwrap().status == LocalStatus::Cancelled
    );
    assert!(
        rb.try_recv()
            .map(|e| !matches!(e, ClientEvent::IncomingReady { .. }))
            .unwrap_or(true)
    );
}

/// 既定チャンクサイズ（8MiB）での複数チャンク転送。小さい chunk_size だけでは
/// Object Storage 側のリクエストサイズ上限などの問題を見逃すため（実際にローカルサーバーの 2MB 上限を踏んだ）
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn default_chunk_size_transfer() {
    let env = env().await;
    let a = env.enroll("a").await;
    let b = env.enroll("b").await;
    let mut rb = b.subscribe();
    a.start();
    b.start();
    wait_online(&mut rb).await;
    let src = env.tmp.path().join("src");
    let f = make_file(&src, "default-chunk.bin", 17 * 1024 * 1024 + 5, 13);
    let t = a
        .send_files(b.endpoint_id(), TransferKind::Files, vec![outfile(&f)], None)
        .await
        .unwrap();
    assert_eq!(t.chunk_size, tsute_proto::DEFAULT_CHUNK_SIZE);
    assert_eq!(t.files[0].chunk_count, 3);
    wait_ready(&mut rb, &t.transfer_id, 60).await;
    let item = b.item(&t.transfer_id).unwrap().unwrap();
    assert_eq!(std::fs::read(&item.paths[0]).unwrap(), std::fs::read(&f).unwrap());
}
