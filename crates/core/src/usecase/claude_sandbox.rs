//! `usagi claude-sandbox` の OS sandbox 起動計画を組む純粋ロジック。
//!
//! Claude は必ず platform sandbox の中で起動する（多層防御の hard boundary）。この module は
//! 「どの backend を、どの引数で exec するか」だけを決める純粋な決定部で、platform 判定・backend
//! の探索・`$TMPDIR` / `$HOME` の読み取り・実 exec は合成ルートが束ねる。ここには値だけが渡り、
//! IO を持たないためユニットテストで全分岐を被覆できる。
//!
//! **fail-closed**: sandbox backend が無い、または未対応 platform では [`SandboxPlan::Reject`] を
//! 返し、Claude を無保護で起動しない。合成ルートは Reject を非 0 終了に写す。
//!
//! writable root は 2 系統を結合する。provisioner が起動 scope から渡す [`SandboxRequest::launch_roots`]
//! （cwd・workspace の `.usagi`・Git common dir・usagi state）と、この module が platform / 環境から
//! 補う普遍領域（`$TMPDIR`・`/tmp`・`/var/tmp`・Claude state・macOS の Keychain / MDS cache）である。
//! sandbox は書き込みだけをこの root 集合に閉じ込め、読み取りは許す（読み取り側の論理境界は
//! [`crate::usecase::workspace_guard`] の `PreToolUse` フックが担う）。

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Component, Path, PathBuf};

/// sandbox を提供する対象 platform。`Unsupported`（Windows など）は fail-closed で拒否する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    /// macOS。backend は `/usr/bin/sandbox-exec`。
    MacOs,
    /// Linux。backend は `bwrap`（bubblewrap）。
    Linux,
    /// sandbox backend を持たない platform。
    Unsupported,
}

/// 起動モード。cwd 由来で合成ルートが判定する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxMode {
    /// session worktree に隔離されたエージェント。
    Session,
    /// workspace root で動くコーディネータ。project root と一時領域だけを書き込み可にする。
    Root,
}

impl SandboxMode {
    /// CLI 引数・診断ラベル用の安定した文字列。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            SandboxMode::Session => "session",
            SandboxMode::Root => "root",
        }
    }
}

/// sandbox 起動計画の入力。合成ルートが実環境から読み取った値をここへ渡す。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxRequest {
    /// 対象 platform。
    pub platform: Platform,
    /// 起動モード。
    pub mode: SandboxMode,
    /// 解決済み backend 実行ファイル（macOS: `sandbox-exec` / Linux: `bwrap`）。無ければ `None`。
    pub backend: Option<PathBuf>,
    /// provisioner が起動 scope から渡す writable root。
    pub launch_roots: Vec<PathBuf>,
    /// `$TMPDIR`（あれば）。
    pub tmpdir: Option<PathBuf>,
    /// `$HOME`（あれば）。Claude state・macOS の Keychain に使う。
    pub home: Option<PathBuf>,
    /// sandbox の中で exec する program と引数（先頭が program、以降が引数）。
    pub command: Vec<String>,
}

/// sandbox 起動計画。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxPlan {
    /// `program` を backend として `argv` で exec する。
    Launch {
        /// exec する backend 実行ファイル。
        program: PathBuf,
        /// backend に渡す完全な引数列（product program を内包する）。
        argv: Vec<String>,
    },
    /// 無保護起動を避けるための fail-closed 拒否。`reason` は診断向け。
    Reject {
        /// 拒否理由（人間向け）。
        reason: String,
    },
}

/// 入力から sandbox 起動計画を決める。backend 不在・未対応 platform・空 command は
/// すべて [`SandboxPlan::Reject`]（fail-closed）。
#[must_use]
pub fn plan(request: &SandboxRequest) -> SandboxPlan {
    let Some((program, program_args)) = request.command.split_first() else {
        return SandboxPlan::Reject {
            reason: "sandbox に渡す command がありません".to_owned(),
        };
    };
    let roots = writable_roots(request);
    match request.platform {
        Platform::Unsupported => SandboxPlan::Reject {
            reason: "このプラットフォームには OS sandbox backend が無いため、Claude を無保護で起動しません"
                .to_owned(),
        },
        Platform::MacOs => match &request.backend {
            None => reject_backend("sandbox-exec"),
            Some(backend) => SandboxPlan::Launch {
                program: backend.clone(),
                argv: macos_argv(request.mode, &roots, program, program_args),
            },
        },
        Platform::Linux => match &request.backend {
            None => reject_backend("bwrap"),
            Some(backend) => SandboxPlan::Launch {
                program: backend.clone(),
                argv: linux_argv(&roots, program, program_args),
            },
        },
    }
}

fn reject_backend(backend: &str) -> SandboxPlan {
    SandboxPlan::Reject {
        reason: format!(
            "sandbox backend（{backend}）が見つからないため、Claude を無保護で起動しません"
        ),
    }
}

/// 起動固有の root（provisioner 由来）と普遍領域を結合し、重複を除いた決定的な writable root 集合。
fn writable_roots(request: &SandboxRequest) -> Vec<PathBuf> {
    let mut roots: BTreeSet<PathBuf> = request.launch_roots.iter().cloned().collect();
    roots.insert(PathBuf::from("/tmp"));
    roots.insert(PathBuf::from("/var/tmp"));
    if let Some(tmpdir) = &request.tmpdir {
        roots.insert(tmpdir.clone());
    }
    if let Some(home) = &request.home {
        // Claude 自身の state / 認証キャッシュ。
        roots.insert(home.join(".claude"));
        if request.platform == Platform::MacOs {
            roots.insert(home.join("Library/Keychains"));
        }
    }
    if request.platform == Platform::MacOs {
        // 認証に使う system Keychain と Metadata (MDS) cache。
        roots.insert(PathBuf::from("/Library/Keychains"));
        roots.insert(PathBuf::from("/private/var/db/mds"));
    }
    roots.into_iter().collect()
}

/// macOS: `sandbox-exec -p <profile> <program> <args…>`。
fn macos_argv(
    mode: SandboxMode,
    roots: &[PathBuf],
    program: &str,
    program_args: &[String],
) -> Vec<String> {
    let mut argv = vec![
        "-p".to_owned(),
        macos_profile(mode, roots),
        program.to_owned(),
    ];
    argv.extend(program_args.iter().cloned());
    argv
}

/// 読み取りは許可し、書き込みを writable root の subpath だけに閉じ込める `sandbox-exec` profile。
fn macos_profile(mode: SandboxMode, roots: &[PathBuf]) -> String {
    let subpaths = macos_write_roots(roots)
        .iter()
        .fold(String::new(), |mut acc, root| {
            // `String` への write! は無謬。
            let _ = writeln!(acc, "  (subpath {})", sandbox_string_literal(root));
            acc
        });
    format!(
        "(version 1)\n;; usagi claude-sandbox mode={}\n(allow default)\n(deny file-write*)\n(allow file-write*\n{subpaths})\n",
        mode.as_str()
    )
}

/// writable root を SBPL subpath 集合へ正規化する。末尾スラッシュ付き subpath はマッチ
/// しないため落とし、macOS の firmlink（`/var` `/tmp` `/etc` → `/private/*`）で実書き込み先に
/// なる `/private` 側も許可する。決定的にするため `BTreeSet` で重複排除・整列する。
fn macos_write_roots(roots: &[PathBuf]) -> BTreeSet<PathBuf> {
    let mut normalized = BTreeSet::new();
    for root in roots {
        let root = strip_trailing_slash(root);
        if let Some(private) = private_firmlink_variant(&root) {
            normalized.insert(private);
        }
        normalized.insert(root);
    }
    normalized
}

/// 末尾スラッシュを除いた path（SBPL subpath は末尾スラッシュ付きだとマッチしない）。
fn strip_trailing_slash(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    let trimmed = text.trim_end_matches('/');
    if trimmed.is_empty() {
        PathBuf::from("/")
    } else {
        PathBuf::from(trimmed)
    }
}

/// macOS で `/private` へ firmlink される top-level（`/var` `/tmp` `/etc`）配下なら、実書き込み
/// 先になる `/private`-prefixed path を返す。それ以外は `None`。
fn private_firmlink_variant(root: &Path) -> Option<PathBuf> {
    let mut components = root.components();
    if components.next() != Some(Component::RootDir) {
        return None;
    }
    let Component::Normal(top) = components.next()? else {
        return None;
    };
    if matches!(top.to_str(), Some("var" | "tmp" | "etc")) {
        Some(Path::new("/private").join(root.strip_prefix("/").ok()?))
    } else {
        None
    }
}

/// `sandbox-exec` profile の文字列リテラル（`"…"`。`\` と `"` を escape する）。
fn sandbox_string_literal(path: &Path) -> String {
    let escaped = path
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// Linux: `bwrap --ro-bind / / … --bind-try <root> <root> … <program> <args…>`。
///
/// root 全体を read-only で束ね、writable root だけを read-write で再 bind する。`--bind-try` は
/// 存在しない root（未作成の Claude state など）でも起動を止めない。
fn linux_argv(roots: &[PathBuf], program: &str, program_args: &[String]) -> Vec<String> {
    let mut argv = vec![
        "--ro-bind".to_owned(),
        "/".to_owned(),
        "/".to_owned(),
        "--dev".to_owned(),
        "/dev".to_owned(),
        "--proc".to_owned(),
        "/proc".to_owned(),
        "--die-with-parent".to_owned(),
    ];
    for root in roots {
        let path = root.to_string_lossy().into_owned();
        argv.push("--bind-try".to_owned());
        argv.push(path.clone());
        argv.push(path);
    }
    argv.push(program.to_owned());
    argv.extend(program_args.iter().cloned());
    argv
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(platform: Platform, backend: Option<&str>) -> SandboxRequest {
        SandboxRequest {
            platform,
            mode: SandboxMode::Session,
            backend: backend.map(PathBuf::from),
            launch_roots: vec![PathBuf::from("/repo/.usagi/sessions/work")],
            tmpdir: Some(PathBuf::from("/tmp/user")),
            home: Some(PathBuf::from("/home/dev")),
            command: vec!["claude".to_owned(), "--print".to_owned()],
        }
    }

    #[test]
    fn empty_command_is_rejected_fail_closed() {
        let mut request = request(Platform::Linux, Some("/usr/bin/bwrap"));
        request.command.clear();
        assert!(
            matches!(plan(&request), SandboxPlan::Reject { reason } if reason.contains("command"))
        );
    }

    // Option を返す accessor で variant を取り出す（`let ... else { panic!() }` の未実行 panic 行を
    // 作らず、`.unwrap()` の panic は std 側に置いて自 crate の行被覆を 100% に保つ）。
    impl SandboxPlan {
        fn into_launch(self) -> Option<(PathBuf, Vec<String>)> {
            match self {
                SandboxPlan::Launch { program, argv } => Some((program, argv)),
                SandboxPlan::Reject { .. } => None,
            }
        }
        fn into_reject(self) -> Option<String> {
            match self {
                SandboxPlan::Reject { reason } => Some(reason),
                SandboxPlan::Launch { .. } => None,
            }
        }
    }

    #[test]
    fn unsupported_platform_never_launches_unprotected() {
        let plan = plan(&request(Platform::Unsupported, None));
        // Reject を into_launch すると None（accessor の Reject 分岐を被覆）。
        assert!(plan.clone().into_launch().is_none());
        assert!(plan.into_reject().unwrap().contains("無保護"));
    }

    #[test]
    fn a_missing_backend_is_rejected_on_each_supported_platform() {
        for (platform, backend) in [
            (Platform::MacOs, "sandbox-exec"),
            (Platform::Linux, "bwrap"),
        ] {
            let reason = plan(&request(platform, None)).into_reject().unwrap();
            assert!(reason.contains(backend), "{platform:?} names its backend");
        }
    }

    #[test]
    fn macos_wraps_claude_with_a_write_confining_profile() {
        let launched = plan(&request(Platform::MacOs, Some("/usr/bin/sandbox-exec")));
        // Launch を into_reject すると None（accessor の Launch 分岐を被覆）。
        assert!(launched.clone().into_reject().is_none());
        let (program, argv) = launched.into_launch().unwrap();
        assert_eq!(program, PathBuf::from("/usr/bin/sandbox-exec"));
        assert_eq!(argv[0], "-p");
        let profile = &argv[1];
        assert!(profile.contains("(deny file-write*)"));
        assert!(profile.contains("mode=session"));
        // 起動固有 root と普遍領域の双方が subpath になる。
        assert!(profile.contains("(subpath \"/repo/.usagi/sessions/work\")"));
        assert!(profile.contains("(subpath \"/tmp\")"));
        assert!(profile.contains("(subpath \"/home/dev/.claude\")"));
        assert!(profile.contains("(subpath \"/home/dev/Library/Keychains\")"));
        assert!(profile.contains("(subpath \"/Library/Keychains\")"));
        // macOS firmlink 側（実書き込み先）も許可する。
        assert!(profile.contains("(subpath \"/private/tmp\")"));
        assert!(profile.contains("(subpath \"/private/var/tmp\")"));
        // program と引数が profile の後ろに続く。
        assert_eq!(&argv[2..], ["claude", "--print"]);
    }

    #[test]
    fn macos_profile_strips_trailing_slashes_and_adds_private_firmlink_variants() {
        let mut request = request(Platform::MacOs, Some("/usr/bin/sandbox-exec"));
        // 末尾スラッシュ付きの macOS 一時ディレクトリ（$TMPDIR の実値に近い形）。
        request.tmpdir = Some(PathBuf::from("/var/folders/ab/T/"));
        request.home = None;
        let (_program, argv) = plan(&request).into_launch().unwrap();
        let profile = &argv[1];
        // 末尾スラッシュは落ち、実書き込み先の /private 側が許可される。
        assert!(profile.contains("(subpath \"/var/folders/ab/T\")"));
        assert!(profile.contains("(subpath \"/private/var/folders/ab/T\")"));
        assert!(!profile.contains("/var/folders/ab/T/\""));
    }

    #[test]
    fn strip_trailing_slash_collapses_to_root_and_drops_suffixes() {
        assert_eq!(
            strip_trailing_slash(Path::new("/tmp/")),
            PathBuf::from("/tmp")
        );
        assert_eq!(strip_trailing_slash(Path::new("/")), PathBuf::from("/"));
        assert_eq!(
            strip_trailing_slash(Path::new("/var/tmp")),
            PathBuf::from("/var/tmp")
        );
    }

    #[test]
    fn private_firmlink_variant_only_expands_firmlinked_tops() {
        assert_eq!(
            private_firmlink_variant(Path::new("/tmp")),
            Some(PathBuf::from("/private/tmp"))
        );
        assert_eq!(
            private_firmlink_variant(Path::new("/var/folders/x")),
            Some(PathBuf::from("/private/var/folders/x"))
        );
        // firmlink されない top、root のみ、非 Normal、相対 path はいずれも None。
        assert_eq!(private_firmlink_variant(Path::new("/repo/src")), None);
        assert_eq!(private_firmlink_variant(Path::new("/")), None);
        assert_eq!(private_firmlink_variant(Path::new("/..")), None);
        assert_eq!(private_firmlink_variant(Path::new("relative/dir")), None);
    }

    #[test]
    fn linux_binds_root_read_only_and_rebinds_writable_roots() {
        let (program, argv) = plan(&request(Platform::Linux, Some("/usr/bin/bwrap")))
            .into_launch()
            .unwrap();
        assert_eq!(program, PathBuf::from("/usr/bin/bwrap"));
        assert_eq!(&argv[..3], ["--ro-bind", "/", "/"]);
        assert!(argv.contains(&"--die-with-parent".to_owned()));
        // writable root は --bind-try で二重指定（SRC DEST）。
        let bind = argv
            .windows(3)
            .any(|w| w[0] == "--bind-try" && w[1] == "/repo/.usagi/sessions/work" && w[2] == w[1]);
        assert!(bind, "launch root is rebound read-write");
        // Linux では Keychain / MDS を writable root にしない。
        assert!(!argv.iter().any(|token| token.contains("Keychains")));
        // program と引数が末尾に来る。
        assert_eq!(&argv[argv.len() - 2..], ["claude", "--print"]);
    }

    #[test]
    fn universal_roots_omit_optional_environment_when_absent() {
        let mut request = request(Platform::Linux, Some("/usr/bin/bwrap"));
        request.launch_roots.clear();
        request.tmpdir = None;
        request.home = None;
        let roots = writable_roots(&request);
        assert!(roots.contains(&PathBuf::from("/tmp")));
        assert!(roots.contains(&PathBuf::from("/var/tmp")));
        // TMPDIR / HOME が無ければ由来 root は増えない。
        assert!(!roots.iter().any(|root| root.ends_with(".claude")));
        assert_eq!(roots.len(), 2);
    }

    #[test]
    fn profile_string_literals_escape_quotes_and_backslashes() {
        let literal = sandbox_string_literal(Path::new(r#"/a"b\c"#));
        assert_eq!(literal, r#""/a\"b\\c""#);
    }

    #[test]
    fn mode_and_derived_types_expose_stable_projections() {
        assert_eq!(SandboxMode::Session.as_str(), "session");
        assert_eq!(SandboxMode::Root.as_str(), "root");
        // derive された Debug / Clone / PartialEq を実行する。
        let plan = SandboxPlan::Reject {
            reason: "x".to_owned(),
        };
        assert_eq!(plan.clone(), plan);
        assert!(format!("{plan:?}").contains("Reject"));
        let request = request(Platform::MacOs, Some("/usr/bin/sandbox-exec"));
        assert_eq!(request.clone(), request);
        assert!(format!("{:?}", Platform::Linux).contains("Linux"));
    }
}
