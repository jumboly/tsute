include!("../../build-support/git_commit.rs");

fn main() {
    emit_git_commit();
    tauri_build::build()
}
