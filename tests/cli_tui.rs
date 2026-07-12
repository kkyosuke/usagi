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
fn routes_commands_to_their_tui_entry_screens() {
    let cases: &[(&[&OsStr], &str)] = &[
        (&[], "welcome TUI"),
        (&[OsStr::new("hop")], "welcome TUI"),
        (&[OsStr::new("config")], "config TUI"),
        (&[OsStr::new("doctor")], "doctor TUI"),
    ];

    for (args, expected) in cases {
        let output = run(args);
        assert!(output.status.success(), "args={args:?}");
        assert!(stdout(&output).contains(expected), "args={args:?}");
        assert!(output.stderr.is_empty(), "args={args:?}");
    }
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
