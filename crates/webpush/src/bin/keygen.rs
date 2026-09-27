//! `tsute-vapid-keygen <out-file>`: VAPID 秘密鍵を生成し、所有者のみ読める権限でファイルに書く。
//! 標準出力に出さないのは、端末のスクロールバックやログに秘密鍵を残さないため。
use std::io::Write;

fn main() {
    let path = std::env::args().nth(1).expect("usage: tsute-vapid-keygen <out-file>");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    let mut f = opts.open(&path).expect("create key file (must not exist)");
    f.write_all(tsute_webpush::generate_private_key().as_bytes())
        .expect("write key");
}
