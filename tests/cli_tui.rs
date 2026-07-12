//! 配布バイナリの CLI 解析から TUI 起動画面までを通す結合テスト。

use std::ffi::OsStr;
use std::process::{Command, Output};

fn run(args: &[&OsStr]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_usagi"))
        .args(args)
        .output()
        .expect("usagi バイナリを起動できる")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn welcome_entry_renders_the_welcome_screen() {
    // 引数なしと `hop` はどちらも welcome 画面を選ぶ。テストでは stdout が tty でないため、
    // 合成ルートは対話ループの代わりに welcome の 1 フレームを描いて返す。
    for args in [&[][..], &[OsStr::new("hop")][..]] {
        let output = run(args);
        assert!(output.status.success(), "args={args:?}");
        let out = stdout(&output);
        assert!(out.contains("USAGI"), "args={args:?}");
        assert!(out.contains("Menu"), "args={args:?}");
        assert!(out.contains("q: quit"), "args={args:?}");
        assert!(output.stderr.is_empty(), "args={args:?}");
    }
}

#[test]
fn daemon_status_reports_not_running_with_a_fresh_data_dir() {
    // `usagi daemon status` を実バイナリで走らせ、合成ルートが束ねる実ストア
    // （`FsRecordFile` を backing にした `DaemonRecordStore`）を通す。データディレクトリを
    // 空の一時パスへ向けるので、レコードは無く「daemon not running」を報告する。
    let home = std::env::temp_dir().join(format!("usagi-daemon-status-{}", std::process::id()));
    let output = Command::new(env!("CARGO_BIN_EXE_usagi"))
        .args([OsStr::new("daemon"), OsStr::new("status")])
        .env("USAGI_HOME", &home)
        .output()
        .expect("usagi バイナリを起動できる");
    assert!(output.status.success());
    assert!(stdout(&output).contains("daemon not running"));
}

#[test]
fn config_entry_renders_the_config_screen() {
    // `usagi config` は Config 画面を選ぶ。stdout が tty でないため、合成ルートは対話ループの
    // 代わりに Config の 1 フレームを描いて返す。
    let output = run(&[OsStr::new("config")]);
    assert!(output.status.success());
    let out = stdout(&output);
    assert!(out.contains("Config"));
    assert!(out.contains("No settings"));
    assert!(out.contains("Esc: back"));
    assert!(output.stderr.is_empty());
}

#[test]
fn other_entries_route_to_their_banner_screens() {
    // 対話ループ未接続の画面（Doctor）は暫定バナー。
    let output = run(&[OsStr::new("doctor")]);
    assert!(output.status.success());
    assert!(stdout(&output).contains("doctor TUI"));
    assert!(output.stderr.is_empty());
}

#[test]
fn open_forwards_an_explicit_or_current_workspace_path() {
    let explicit = std::env::temp_dir().join("usagi-cli-tui-explicit");
    let output = run(&[OsStr::new("open"), explicit.as_os_str()]);
    assert!(output.status.success());
    let out = stdout(&output);
    assert!(out.contains("workspace TUI"));
    assert!(out.contains(&explicit.display().to_string()));

    let current = std::env::current_dir().unwrap();
    let output = run(&[OsStr::new("open")]);
    assert!(output.status.success());
    let out = stdout(&output);
    assert!(out.contains("workspace TUI"));
    assert!(out.contains(&current.display().to_string()));
}

#[test]
fn clap_errors_do_not_launch_a_tui() {
    for args in [
        &[OsStr::new("hop"), OsStr::new("extra")][..],
        &[OsStr::new("config"), OsStr::new("extra")][..],
        &[OsStr::new("open"), OsStr::new("one"), OsStr::new("two")][..],
    ] {
        let output = run(args);
        assert!(!output.status.success(), "args={args:?}");
        assert!(!stdout(&output).contains("TUI"), "args={args:?}");
        assert!(!output.stderr.is_empty(), "args={args:?}");
    }
}

#[cfg(unix)]
#[test]
fn open_accepts_a_non_utf8_workspace_path() {
    use std::os::unix::ffi::OsStringExt;

    let path = std::ffi::OsString::from_vec(b"/tmp/usagi-\xff".to_vec());
    let output = run(&[OsStr::new("open"), &path]);

    assert!(output.status.success());
    assert!(stdout(&output).contains("workspace TUI"));
}
