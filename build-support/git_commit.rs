// build.rs から include! する共有コード。ビルドしたコミットを `TSUTE_GIT_COMMIT`（短いハッシュ）として埋め込み、
// 画面や /api/health で「どの版が動いているか」を確かめられるようにする。
//
// 優先順: 環境変数 TSUTE_GIT_COMMIT（明示指定）→ GITHUB_SHA（CI）→ git コマンド（手元）。
// 手元で未コミットの変更があるときは `-dirty` を付け、コミットと中身が一致しないことを区別する。
// git が無い環境（ソース tarball 等）では "unknown"。

fn emit_git_commit() {
    println!("cargo:rerun-if-env-changed=TSUTE_GIT_COMMIT");
    println!("cargo:rerun-if-env-changed=GITHUB_SHA");
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    // コミット・ステージの変化で再実行する（HEAD はブランチを指すだけでコミットでは変わらないため index も見る）
    if let Some(dir) = git(&["rev-parse", "--absolute-git-dir"]) {
        for f in ["HEAD", "index"] {
            println!("cargo:rerun-if-changed={dir}/{f}");
        }
    }
    let short = |s: &str| s.chars().take(7).collect::<String>();
    let commit = if let Some(v) = std::env::var("TSUTE_GIT_COMMIT").ok().filter(|v| !v.is_empty()) {
        v
    } else if let Some(v) = std::env::var("GITHUB_SHA").ok().filter(|v| !v.is_empty()) {
        short(&v)
    } else if let Some(v) = git(&["rev-parse", "--short=7", "HEAD"]) {
        let dirty = git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty());
        if dirty { format!("{v}-dirty") } else { v }
    } else {
        "unknown".to_string()
    };
    println!("cargo:rustc-env=TSUTE_GIT_COMMIT={commit}");
}
