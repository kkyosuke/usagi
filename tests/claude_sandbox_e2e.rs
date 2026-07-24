//! 出荷バイナリの `usagi claude-sandbox` が、writable root の外へ**決して**書かせないことの結合テスト。
//!
//! 期待は platform に依存しない。backend（macOS の `sandbox-exec` / Linux の `bwrap`）があれば
//! sandbox が書き込みを拒否し、backend が無ければ launcher が起動自体を拒否する（fail-closed）。
//! どちらでも「無保護で書き込まれた成果物は残らない」という同じ観測になるため、`bwrap` を持たない
//! Linux CI でも分岐なしに検証できる。

#![cfg(unix)]

use std::fs;
use std::path::PathBuf;
use std::process::Command;

/// writable root の外側にあたる作業ディレクトリ。`/tmp` `/var/tmp` `$TMPDIR` は launcher が普遍領域
/// として常に writable にするため、それらの下では「拒否される書き込み先」を作れない。ビルド成果物の
/// ディレクトリは launch root にも普遍領域にも含まれないので、この検証の外側として使える。
fn outside_writable_roots() -> PathBuf {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("claude-sandbox-e2e");
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(&directory).expect("build-artifact scratch directory is creatable");
    directory
}

#[test]
fn the_launcher_never_lets_a_child_write_outside_its_writable_roots() {
    let outside = outside_writable_roots();
    let denied = outside.join("denied");
    let allowed = tempfile::Builder::new()
        .prefix("usagi-sandbox-")
        .tempdir_in("/tmp")
        .expect("short writable root");

    let status = Command::new(env!("CARGO_BIN_EXE_usagi"))
        .args(["claude-sandbox", "--mode", "session", "--writable-root"])
        .arg(allowed.path())
        .arg("--")
        .args(["/bin/sh", "-c"])
        .arg(format!("echo escaped > {}", denied.display()))
        .status()
        .expect("shipping launcher starts");

    // backend があれば書き込みが deny され、無ければ launcher が拒否する。いずれも非 0 終了で、
    // 成果物は残らない。
    assert!(!status.success(), "unprotected launch must never succeed");
    assert!(
        !denied.exists(),
        "a write outside every writable root must not land"
    );
    let _ = fs::remove_dir_all(&outside);
}
