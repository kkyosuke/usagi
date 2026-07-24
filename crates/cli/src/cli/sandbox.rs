//! すべての Claude 起動を包む fail-closed な OS sandbox launcher の純粋ロジック。
//!
//! `usagi claude-sandbox` は `claude` を直接起動する代わりにこの経路を通す。writable root を
//! canonicalize してから platform sandbox の起動コマンドを組み立て、sandbox プロセスが起動して
//! 初めて要求コマンドを実行する。**利用不能時や未対応 platform では無保護フォールバックせず起動を
//! 拒否する**（fail-closed）。
//!
//! この module は純粋な組み立て（[`Mode`] / [`platform_command`] / [`canonical_roots`] /
//! [`writable_temp_roots`]）だけを持つ。writable root の集約、実 cwd / home の解決、macOS の
//! 継承 sandbox 判定、実プロセス起動と終了 status の確認は合成ルートが束ねる。

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::cli::{Run, RunOutcome};

/// `usagi claude-sandbox` のハンドラ。解析済みの mode / writable root / コマンドを保持し、
/// 実 cwd・home の解決と実プロセス起動は合成ルートが束ねる（[`RunOutcome::ClaudeSandbox`]）。
pub struct ClaudeSandbox {
    pub mode: String,
    pub writable_roots: Vec<PathBuf>,
    pub command: Vec<OsString>,
}

impl Run for ClaudeSandbox {
    fn run(&self, _out: &mut dyn Write) -> io::Result<RunOutcome> {
        Ok(RunOutcome::ClaudeSandbox {
            mode: self.mode.clone(),
            writable_roots: self.writable_roots.clone(),
            command: self.command.clone(),
        })
    }
}

/// 起動 cwd そのものを書き込み可能にするか。`session` は現在の session worktree、`root` は
/// project root を writable にする（どちらも意図した作業領域）。cwd 自体の扱いは同じで、区別は
/// 呼び出し側が渡す writable root の集合で表す。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Session,
    Root,
}

impl Mode {
    /// `session` / `root` 以外は拒否する（fail-closed）。
    ///
    /// # Errors
    ///
    /// 未知の mode 文字列のときエラー文字列を返す。
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "session" => Ok(Self::Session),
            "root" => Ok(Self::Root),
            other => Err(format!("invalid Claude sandbox mode: {other}")),
        }
    }
}

/// platform sandbox を起動する具体コマンド（program と引数）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxCommand {
    pub program: PathBuf,
    pub args: Vec<OsString>,
}

/// コマンドラインツールが書き込みうる従来の一時領域。
///
/// `temp_dir()` は platform 固有の `$TMPDIR`（macOS の per-user `/var/folders/.../T` など）を保つ。
/// Unix は共有の `/tmp`・`/var/tmp` も持つ。存在しない root は除くので、変わったホストでも起動を
/// 妨げない。
#[must_use]
pub fn writable_temp_roots() -> Vec<PathBuf> {
    let mut roots = vec![std::env::temp_dir()];
    #[cfg(unix)]
    roots.extend(["/tmp", "/var/tmp"].into_iter().map(PathBuf::from));
    roots.retain(|root| root.is_dir());
    roots
}

/// 与えた writable root を canonicalize（symlink 解決）し、重複を除いて返す。
///
/// # Errors
///
/// いずれかの root を canonicalize できない場合にエラー文字列を返す（存在しない root や
/// 解決不能な path は起動を拒否する材料になる）。
pub fn canonical_roots(roots: Vec<PathBuf>) -> Result<Vec<PathBuf>, String> {
    let mut canonical = Vec::new();
    for root in roots {
        let resolved = std::fs::canonicalize(&root).map_err(|error| {
            format!(
                "Claude sandbox writable root cannot be canonicalized: {} ({error})",
                root.display()
            )
        })?;
        if !canonical.contains(&resolved) {
            canonical.push(resolved);
        }
    }
    Ok(canonical)
}

/// writable root だけを許可する Seatbelt profile 付きの `sandbox-exec` コマンドを組み立てる。
///
/// # Errors
///
/// `/usr/bin/sandbox-exec` が見つからないときにエラー文字列を返す（無保護フォールバックしない）。
#[cfg(target_os = "macos")]
pub fn platform_command(
    writable_roots: &[PathBuf],
    command: &[OsString],
) -> Result<SandboxCommand, String> {
    use std::fmt::Write as _;

    let sandbox_exec = PathBuf::from("/usr/bin/sandbox-exec");
    if !sandbox_exec.is_file() {
        return Err(
            "Claude OS sandbox is unavailable: /usr/bin/sandbox-exec was not found".to_string(),
        );
    }
    let mut allowed = String::new();
    for root in writable_roots {
        let path = root.to_string_lossy();
        let escaped = path.replace('\\', "\\\\").replace('"', "\\\"");
        let _ = write!(allowed, " (subpath \"{escaped}\")");
    }
    let deny = if allowed.is_empty() {
        "(deny file-write*)".to_string()
    } else {
        format!("(deny file-write* (require-not (require-any{allowed})))")
    };
    let profile = format!("(version 1)\n(allow default)\n{deny}\n");
    let mut args = vec![OsString::from("-p"), OsString::from(profile)];
    args.extend_from_slice(command);
    Ok(SandboxCommand {
        program: sandbox_exec,
        args,
    })
}

/// ホストを read-only で bind し、writable root だけを read-write で bind する `bwrap` コマンドを
/// 組み立てる。
///
/// # Errors
///
/// 現状は失敗しないが、backend 差異を吸収するため他 platform と同じ `Result` 型に揃える。
#[cfg(target_os = "linux")]
pub fn platform_command(
    writable_roots: &[PathBuf],
    command: &[OsString],
) -> Result<SandboxCommand, String> {
    let mut args: Vec<OsString> = [
        "--die-with-parent",
        "--new-session",
        "--ro-bind",
        "/",
        "/",
        "--dev-bind",
        "/dev",
        "/dev",
        "--proc",
        "/proc",
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    for root in writable_roots {
        args.push(OsString::from("--bind"));
        args.push(root.as_os_str().to_owned());
        args.push(root.as_os_str().to_owned());
    }
    args.push(OsString::from("--"));
    args.extend_from_slice(command);
    Ok(SandboxCommand {
        program: PathBuf::from("bwrap"),
        args,
    })
}

/// 対応 backend の無い platform。無保護フォールバックせず常にエラーにする。
///
/// # Errors
///
/// 常にエラー文字列を返す（未対応 platform では Claude を起動しない）。
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn platform_command(
    _writable_roots: &[PathBuf],
    _command: &[OsString],
) -> Result<SandboxCommand, String> {
    Err(
        "Claude OS sandbox is unsupported on this platform; refusing unprotected launch"
            .to_string(),
    )
}

/// macOS の Keychain を sandbox 内で使えるようにする writable root。
///
/// Claude は OAuth credential を login Keychain に置く。読み取りには Module Directory Service が
/// `$DARWIN_USER_CACHE_DIR` 配下に per-user cache を維持する必要があり、token refresh は
/// `~/Library/Keychains` の keychain DB を書き換える。どちらかが拒否されると Claude は Keychain に
/// 到達できず、失効しうる `~/.claude/.credentials.json` にフォールバックして認証エラーで終了する。
///
/// # Errors
///
/// Darwin user cache directory を解決できないときにエラー文字列を返す。
#[cfg(target_os = "macos")]
pub fn macos_keychain_roots(home: &Path) -> Result<Vec<PathBuf>, String> {
    Ok(vec![
        home.join("Library/Keychains"),
        darwin_user_cache_dir()?,
    ])
}

/// per-user の Darwin cache directory（`confstr(_CS_DARWIN_USER_CACHE_DIR)`、
/// 例 `/var/folders/<xx>/<id>/C/`）。
///
/// # Errors
///
/// `confstr` が解決に失敗したときにエラー文字列を返す。
#[cfg(target_os = "macos")]
pub fn darwin_user_cache_dir() -> Result<PathBuf, String> {
    use std::os::unix::ffi::OsStringExt;

    let mut buf = vec![0u8; libc::PATH_MAX as usize];
    // SAFETY: buffer は呼び出しより長生きし、容量も一緒に渡している。
    let len = unsafe {
        libc::confstr(
            libc::_CS_DARWIN_USER_CACHE_DIR,
            buf.as_mut_ptr().cast(),
            buf.len(),
        )
    };
    if len == 0 || len > buf.len() {
        return Err("Claude sandbox cannot resolve the Darwin user cache directory".to_string());
    }
    // `len` は末尾 NUL を含むため PathBuf には持ち込まない。
    buf.truncate(len - 1);
    Ok(PathBuf::from(OsString::from_vec(buf)))
}

/// 現在のプロセスが nested な `sandbox-exec` を拒否する macOS Seatbelt profile を既に持つか
/// （usagi 自身が sandbox 化ホスト（例: Codex）から起動された場合など）を、既知の拒否メッセージで判定する。
#[cfg(target_os = "macos")]
#[must_use]
pub fn is_nested_sandbox_rejection(stderr: &[u8]) -> bool {
    String::from_utf8_lossy(stderr).contains("sandbox_apply: Operation not permitted")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn hidden_handler_requests_composition_launch_with_its_arguments() {
        use crate::cli::{Command, RunOutcome, execute};
        let (outcome, output) = execute(Command::ClaudeSandbox {
            mode: "session".into(),
            writable_roots: vec![PathBuf::from("/repo/.usagi")],
            command: vec![OsString::from("claude"), OsString::from("--print")],
        });
        assert_eq!(
            outcome,
            RunOutcome::ClaudeSandbox {
                mode: "session".into(),
                writable_roots: vec![PathBuf::from("/repo/.usagi")],
                command: vec![OsString::from("claude"), OsString::from("--print")],
            }
        );
        assert!(output.is_empty());
    }

    #[test]
    fn mode_parser_is_fail_closed() {
        assert_eq!(Mode::parse("session").unwrap(), Mode::Session);
        assert_eq!(Mode::parse("root").unwrap(), Mode::Root);
        assert!(Mode::parse("unknown").is_err());
    }

    #[test]
    fn canonical_roots_resolve_symlinks_dedupe_and_reject_missing_paths() {
        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real");
        fs::create_dir(&real).unwrap();
        let link = temp.path().join("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&real, &link).unwrap();

        let resolved = fs::canonicalize(&real).unwrap();
        // symlink と実体は同じ canonical path に畳まれ、重複は 1 つに正規化される。
        assert_eq!(canonical_roots(vec![link, real]).unwrap(), vec![resolved]);
        assert!(canonical_roots(vec![temp.path().join("missing")]).is_err());
    }

    #[test]
    fn temporary_roots_include_the_platform_temp_directory() {
        let roots = writable_temp_roots();
        assert!(roots.contains(&std::env::temp_dir()));
        // 返す root はすべて実在ディレクトリ（`retain(is_dir)` を通っている）。
        assert!(roots.iter().all(|root| root.is_dir()));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_plan_mounts_the_host_read_only_then_binds_allow_roots() {
        let root = PathBuf::from("/tmp/allowed");
        let command =
            platform_command(std::slice::from_ref(&root), &[OsString::from("claude")]).unwrap();
        assert_eq!(command.program, Path::new("bwrap"));
        assert!(command.args.windows(3).any(|args| {
            args == [
                OsString::from("--bind"),
                root.clone().into(),
                root.clone().into(),
            ]
        }));
        assert!(command.args.windows(3).any(|args| {
            args == [
                OsString::from("--ro-bind"),
                OsString::from("/"),
                OsString::from("/"),
            ]
        }));
        // 要求コマンドは `--` の後ろに置かれる。
        assert_eq!(command.args.last().unwrap(), &OsString::from("claude"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_profile_allows_only_canonical_write_roots() {
        let temp = tempfile::tempdir().unwrap();
        let command = platform_command(
            &[temp.path().to_path_buf()],
            &[OsString::from("/usr/bin/true")],
        )
        .unwrap();
        assert_eq!(command.program, Path::new("/usr/bin/sandbox-exec"));
        let profile = command.args[1].to_string_lossy();
        assert!(profile.contains("(deny file-write* (require-not (require-any"));
        assert!(profile.contains(&*temp.path().to_string_lossy()));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_profile_denies_all_writes_when_no_root_is_allowed() {
        let command = platform_command(&[], &[OsString::from("/usr/bin/true")]).unwrap();
        let profile = command.args[1].to_string_lossy();
        assert!(profile.contains("(deny file-write*)"));
        assert!(!profile.contains("require-not"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn nested_sandbox_probe_accepts_only_the_known_seatbelt_rejection() {
        assert!(is_nested_sandbox_rejection(
            b"sandbox-exec: sandbox_apply: Operation not permitted\n"
        ));
        assert!(!is_nested_sandbox_rejection(
            b"sandbox-exec: profile invalid\n"
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn keychain_roots_cover_the_keychain_db_and_the_mds_cache() {
        let temp = tempfile::tempdir().unwrap();
        let roots = macos_keychain_roots(temp.path()).unwrap();
        assert_eq!(roots[0], temp.path().join("Library/Keychains"));
        let cache = &roots[1];
        assert!(cache.is_absolute());
        assert!(cache.is_dir());
        assert!(!cache.as_os_str().to_string_lossy().ends_with('\0'));
    }
}
