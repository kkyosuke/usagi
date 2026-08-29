//! `usagi claude-sandbox` の OS sandbox 起動計画を組む純粋ロジック。
//!
//! Claude は必ず platform sandbox の中で起動する（多層防御の hard boundary）。この module は
//! 「どの backend を、どの引数で exec するか」だけを決める純粋な決定部で、platform 判定・backend
//! の bootstrap 解決・policy path の検証・実 exec は合成ルートが束ねる。ここには値だけが渡り、
//! IO を持たないためユニットテストで全分岐を被覆できる。
//!
//! **fail-closed**: sandbox backend が無い、または未対応 platform では [`SandboxPlan::Reject`] を
//! 返し、Claude を無保護で起動しない。合成ルートは Reject を非 0 終了に写す。
//!
//! 起動固有の writable root は provisioner が渡す（session は own worktree と必要な Git administrative
//! state、root coordinator は repository-local root を持たない）。**その起動固有 root に、両 mode とも同じ普遍領域**
//! （`$TMPDIR`・`/tmp`・`/var/tmp`・起動する agent CLI 自身の state と global config・macOS の
//! Keychain と system / per-user の MDS cache）を加える。daemon の再起動は sandbox 外の bootstrap broker に
//! 委譲し、data home は writable root に含めない。sandbox は書き込みだけをこの root 集合に
//! 閉じ込め、読み取りは許す（読み取り側の論理境界は
//! [`crate::usecase::workspace_guard`] の `PreToolUse` フックが担う）。
//!
//! 普遍領域を session から落とすと、agent CLI は**その worktree の中でしか動けない**という以前に
//! **起動できない**。Claude Code は tool を実行するたびに `$TMPDIR` を無視した固定 path
//! （`/tmp/claude-<uid>/<cwd の slug>`）へ scratchpad を作るため、`/tmp` を落とすと全 tool 呼び出しが
//! `EPERM: operation not permitted, mkdir` で失敗する。`~/.claude` を落とすと theme・permission
//! mode といった settings が毎起動リセットされ、macOS の Keychain / MDS を落とすと認証が 401 で
//! 失敗する。state directory だけでは足りない: Claude の onboarding 完了・folder trust・MCP 承認は
//! **`~/.claude` の中ではなく隣の `~/.claude.json`** にあり、しかも保存は lock（`.lock`）と
//! temp file（`.tmp.<pid>.<random>`）を `$HOME` 直下に作って rename で被せる形で行うため、
//! grant は [`agent_config_prefix`] の **path prefix** でなければならない。この prefix が無いと
//! 設定は読めるのに書けず、folder trust と初回フローが毎起動やり直しになる。**session と root を分けるのは repository への書き込み境界**
//! （起動固有 root と `protected_root`）であって、agent 自身の scratch / state / 認証領域ではない。
//!
//! macOS の Keychain 検索は Module Directory Service (MDS) の cache を更新するため、system の
//! `/private/var/db/mds` だけでなく **per-user cache**（`$DARWIN_USER_CACHE_DIR/mds`）にも書ける
//! 必要がある。これが無いと `SecKeychainSearchCreateFromAttributes` が "A Module Directory Service
//! error has occurred." で失敗し、agent CLI は Keychain の credential を読めないまま古い file 側の
//! credential へ fallback して認証エラー（401）で起動できなくなる。
//!
//! agent state は `~/.claude` 固定ではなく、[`agent_state_directory`] が **exec する program**
//! から決める（Claude なら `~/.claude`、Codex なら `~/.codex`、sakana.ai なら `~/.codex-fugu`）。
//! 固定していた間、root の Codex は自分の state DB（`~/.codex/state_5.sqlite`）へ書けず
//! 「attempt to write a readonly database」で起動できなかった。
//!
//! 唯一の例外は [`SandboxRequest::passthrough`] で、これは E2E テスト専用の seam である。
//! [`passthrough_requested`] が唯一の判定点で、shipping（release）ビルドでは常に false を返すため、
//! 配布バイナリにこの迂回路は存在しない。

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Component, Path, PathBuf};

use crate::domain::settings::DefaultModel;

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
    /// workspace root で動くコーディネータ。repository は read-only にする。
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

/// [`SandboxRequest::passthrough`] を要求するテスト専用の環境変数名。
///
/// live な Claude 起動経路（daemon の provisioner）は自身の環境にこの変数があるときだけ、launcher の
/// 子プロセス環境へ同じ変数を注入する。launcher 側は [`passthrough_requested`] で最終判定する。
pub const PASSTHROUGH_ENVIRONMENT_VARIABLE: &str = "USAGI_CLAUDE_SANDBOX_PASSTHROUGH";

/// 拘束を省いて product をそのまま exec してよいか（E2E テスト専用 seam）を決める。
///
/// `bwrap` を持たない Linux CI でも live な Claude 起動経路（launcher・`--settings` フック・PTY
/// ライフサイクル）を E2E で通すための seam である。**shipping ビルドには存在しない**: `debug_build`
/// は合成ルートが `cfg!(debug_assertions)` を渡すため、release ビルドでは環境変数があっても常に
/// false になる。値は厳密に `"1"` のときだけ有効で、空文字や他の値では拘束を外さない。
#[must_use]
pub fn passthrough_requested(debug_build: bool, value: Option<&str>) -> bool {
    debug_build && value == Some("1")
}

/// sandbox 起動計画の入力。合成ルートが実環境から読み取った値をここへ渡す。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxRequest {
    /// 対象 platform。
    pub platform: Platform,
    /// 起動モード。
    pub mode: SandboxMode,
    /// writable root と重ねてはならない workspace root。
    pub protected_root: Option<PathBuf>,
    /// 解決済み backend 実行ファイル（macOS: `sandbox-exec` / Linux: `bwrap`）。無ければ `None`。
    pub backend: Option<PathBuf>,
    /// provisioner が起動 scope から渡す writable root。
    pub launch_roots: Vec<PathBuf>,
    /// `$TMPDIR`（あれば）。
    pub tmpdir: Option<PathBuf>,
    /// `$HOME`（あれば）。Claude state・macOS の Keychain に使う。
    pub home: Option<PathBuf>,
    /// Linux で `$HOME` の writable prefix を表現するため、合成ルートが起動直前に列挙した
    /// `$HOME` 直下の entry。一覧を取得できなければ `None` のまま fail closed にする。
    pub linux_home_entries: Option<Vec<PathBuf>>,
    /// macOS の per-user cache root（`$DARWIN_USER_CACHE_DIR`。あれば）。Keychain 検索が更新する
    /// MDS cache（`<cache>/mds`）を writable にするために使う。
    pub cache_dir: Option<PathBuf>,
    /// テスト専用 seam。true なら backend で包まず command をそのまま exec する。合成ルートは
    /// [`passthrough_requested`] の結果だけをここへ入れる（release ビルドでは常に false）。
    pub passthrough: bool,
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
///
/// [`SandboxRequest::passthrough`] は唯一の迂回路で、これだけが backend を要求せずに command を
/// そのまま exec する計画を返す。合成ルートは [`passthrough_requested`] を通した値しか渡さないため、
/// release ビルドでこの分岐に入ることはない。
#[must_use]
pub fn plan(request: &SandboxRequest) -> SandboxPlan {
    let Some((program, program_args)) = request.command.split_first() else {
        return SandboxPlan::Reject {
            reason: "sandbox に渡す command がありません".to_owned(),
        };
    };
    if request.passthrough {
        return SandboxPlan::Launch {
            program: PathBuf::from(program),
            argv: program_args.to_vec(),
        };
    }
    if let Some(reason) = invalid_policy_reason(request) {
        return SandboxPlan::Reject { reason };
    }
    let roots = writable_roots(request);
    let prefixes = writable_prefixes(request);
    match request.platform {
        Platform::Unsupported => SandboxPlan::Reject {
            reason: "このプラットフォームには OS sandbox backend が無いため、Claude を無保護で起動しません"
                .to_owned(),
        },
        Platform::MacOs => match &request.backend {
            None => reject_backend("sandbox-exec"),
            Some(backend) => SandboxPlan::Launch {
                program: backend.clone(),
                argv: macos_argv(request.mode, &roots, &prefixes, program, program_args),
            },
        },
        Platform::Linux => match &request.backend {
            None => reject_backend("bwrap"),
            Some(backend) => SandboxPlan::Launch {
                program: backend.clone(),
                argv: linux_argv(
                    &roots,
                    &prefixes,
                    request.home.as_deref(),
                    request.linux_home_entries.as_deref(),
                    program,
                    program_args,
                ),
            },
        },
    }
}

fn invalid_policy_reason(request: &SandboxRequest) -> Option<String> {
    if request
        .backend
        .as_ref()
        .is_some_and(|backend| !backend.is_absolute())
    {
        return Some("sandbox backend が absolute path ではありません".to_owned());
    }
    let protected = request.protected_root.as_deref();
    for root in writable_roots(request) {
        if !root.is_absolute() || root == Path::new("/") {
            return Some("writable root が安全な absolute path ではありません".to_owned());
        }
        let overlaps_workspace = protected.is_some_and(|workspace| {
            workspace.starts_with(&root)
                || (request.mode == SandboxMode::Root && root.starts_with(workspace))
        });
        if overlaps_workspace {
            return Some("writable root が保護対象 workspace の ancestor です".to_owned());
        }
    }
    for prefix in writable_prefixes(request) {
        if !prefix.is_absolute() || prefix.to_str().is_none() {
            return Some("writable prefix が安全な absolute path ではありません".to_owned());
        }
        let overlaps_workspace = protected.is_some_and(|workspace| {
            path_has_prefix(workspace, &prefix)
                || (request.mode == SandboxMode::Root && prefix.starts_with(workspace))
        });
        if overlaps_workspace {
            return Some("writable prefix が保護対象 workspace と重なります".to_owned());
        }
    }
    if request.platform == Platform::Linux && !writable_prefixes(request).is_empty() {
        let (Some(home), Some(entries)) = (
            request.home.as_deref(),
            request.linux_home_entries.as_deref(),
        ) else {
            return Some("Linux HOME entry inventory がありません".to_owned());
        };
        if home == Path::new("/")
            || home.to_str().is_none()
            || protected.is_some_and(|workspace| home.starts_with(workspace))
        {
            return Some("Linux HOME が安全な writable directory ではありません".to_owned());
        }
        if entries.iter().any(|entry| {
            !entry.is_absolute() || entry.to_str().is_none() || entry.parent() != Some(home)
        }) {
            return Some("Linux HOME entry inventory が安全な直下 path ではありません".to_owned());
        }
    }
    None
}

fn reject_backend(backend: &str) -> SandboxPlan {
    SandboxPlan::Reject {
        reason: format!(
            "sandbox backend（{backend}）が見つからないため、Claude を無保護で起動しません"
        ),
    }
}

/// exec する program が自身の state / 認証キャッシュを書く `$HOME` 配下の directory 名。
///
/// 根拠は launcher が実際に exec する program（`command` の先頭）だけで、値の単一情報源は
/// [`DefaultModel::state_directory`] である。したがって grant は起動する CLI と必ず一致し、
/// provider を増やしても sandbox 側に写し漏れが起きない。usagi が launch しない未知 program
/// には state root を与えない（fail-closed）。
#[must_use]
pub fn agent_state_directory(program: &str) -> Option<&'static str> {
    let name = Path::new(program).file_name()?;
    DefaultModel::from_selector(&name.to_string_lossy()).map(DefaultModel::state_directory)
}

/// exec する program が `$HOME` 直下へ書く global config の path **prefix**。
///
/// 値の単一情報源は [`DefaultModel::global_config_prefix`] で、[`agent_state_directory`] と同じく
/// 起動する CLI からだけ決まる。Claude の global config は state directory（`~/.claude`）の中では
/// なく `~/.claude.json` にあり、保存は `~/.claude.json.lock` を取って
/// `~/.claude.json.tmp.<pid>.<random>` を書き、それを rename で被せる形で行う（`.backup.<ms>` も
/// 隣に置く）。したがって grant は「その 1 ファイル」ではなく **prefix** でなければならず、
/// prefix が無いと onboarding・folder trust・MCP 承認が毎起動やり直しになる。
/// 自分の state directory の中に config を持つ provider（Codex / `codex-fugu`）は `None`。
#[must_use]
pub fn agent_config_prefix(program: &str) -> Option<&'static str> {
    let name = Path::new(program).file_name()?;
    DefaultModel::from_selector(&name.to_string_lossy())
        .and_then(DefaultModel::global_config_prefix)
}

/// macOS の Keychain 検索が更新する per-user MDS cache（`<cache>/mds`）。
///
/// cache root から実際に writable にする path の導出はここが正本で、純粋な計画側と daemon 側の
/// policy gate が同じ 1 か所を見る（gate だけが親の cache root を見ていると、grant と検証の対象が
/// 静かにずれる）。
#[must_use]
pub fn macos_mds_cache_root(cache_dir: &Path) -> PathBuf {
    cache_dir.join("mds")
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
        // 起動する agent CLI 自身の state / 認証キャッシュ（`~/.claude` / `~/.codex` / `~/.codex-fugu`）。
        if let Some(state) = request
            .command
            .first()
            .and_then(|program| agent_state_directory(program))
        {
            roots.insert(home.join(state));
        }
        if request.platform == Platform::MacOs {
            roots.insert(home.join("Library/Keychains"));
        }
    }
    if request.platform == Platform::MacOs {
        // 認証に使う system Keychain と Metadata (MDS) cache。
        roots.insert(PathBuf::from("/Library/Keychains"));
        roots.insert(PathBuf::from("/private/var/db/mds"));
        // per-user の MDS cache。Keychain 検索はここを更新するため、system 側だけでは
        // "A Module Directory Service error has occurred." で検索が失敗する。
        if let Some(cache) = &request.cache_dir {
            roots.insert(macos_mds_cache_root(cache));
        }
    }
    roots.into_iter().collect()
}

/// 起動する agent CLI の global config が使う writable path prefix 集合。
///
/// [`writable_roots`](writable_roots) が返す root は directory subtree の grant である。config は
/// `$HOME` 直下の**兄弟 file 群**（`.claude.json` 本体・`.lock`・`.tmp.<pid>.<random>`・
/// `.backup.<ms>`）なので、`$HOME` 全体を writable にせずに保存を通すには prefix 単位の grant が要る。
fn writable_prefixes(request: &SandboxRequest) -> Vec<PathBuf> {
    let mut prefixes: BTreeSet<PathBuf> = BTreeSet::new();
    if let Some(home) = &request.home
        && let Some(prefix) = request
            .command
            .first()
            .and_then(|program| agent_config_prefix(program))
    {
        prefixes.insert(home.join(prefix));
    }
    prefixes.into_iter().collect()
}

/// macOS: `sandbox-exec -p <profile> <program> <args…>`。
fn macos_argv(
    mode: SandboxMode,
    roots: &[PathBuf],
    prefixes: &[PathBuf],
    program: &str,
    program_args: &[String],
) -> Vec<String> {
    let mut argv = vec![
        "-p".to_owned(),
        macos_profile(mode, roots, prefixes),
        program.to_owned(),
    ];
    argv.extend(program_args.iter().cloned());
    argv
}

/// 読み取りは許可し、書き込みを writable root の subpath だけに閉じ込める `sandbox-exec` profile。
///
/// 最後の 1 行だけが例外で、`/dev` 配下の device node への **data 書き込み**を許す。`/dev/null` を
/// `O_RDWR` で開けないと `git` すら
/// "fatal: could not open '/dev/null' for reading and writing" で失敗するためである
/// （Linux の `bwrap` は `--dev /dev` で新しい devtmpfs を張るため、この差は macOS だけに出る）。
///
/// `(literal "/dev/null")` のような列挙にしないのは、agent が動かす shell が `> /dev/stdout` や
/// `> /dev/fd/1` を日常的に使い、literal 列挙ではそれらが `Operation not permitted` になるためである。
/// 代わりに動詞を `file-write-data` に絞ってあるので、`/dev` への node 作成・削除・属性変更は
/// deny のまま残る。
fn macos_profile(mode: SandboxMode, roots: &[PathBuf], prefixes: &[PathBuf]) -> String {
    let subpaths = macos_write_roots(roots)
        .iter()
        .fold(String::new(), |mut acc, root| {
            // `String` への write! は無謬。
            let _ = writeln!(acc, "  (subpath {})", sandbox_string_literal(root));
            acc
        });
    let prefix_rules = macos_write_roots(prefixes)
        .iter()
        .fold(String::new(), |mut acc, prefix| {
            let _ = writeln!(
                acc,
                "(allow file-write* (regex {}))",
                sandbox_regex_prefix_literal(prefix)
            );
            acc
        });
    format!(
        "(version 1)\n;; usagi claude-sandbox mode={}\n(allow default)\n(deny file-write*)\n(allow file-write*\n{subpaths})\n{prefix_rules}(allow file-write-data (subpath \"/dev\"))\n",
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

/// `sandbox-exec` profile の regex リテラル（`#"^…"`）。path を literal として扱うため、
/// 英数字・`/`・`_`・`-` 以外はすべて backslash で escape する（`.` を素で通すと任意 1 文字に
/// なり、`"` を素で通すと literal が途中で閉じる）。先頭 `^` を付けて **prefix 一致**にする。
fn sandbox_regex_prefix_literal(path: &Path) -> String {
    let escaped = path
        .to_string_lossy()
        .chars()
        .fold(String::new(), |mut acc, character| {
            if !character.is_ascii_alphanumeric() && !matches!(character, '/' | '_' | '-') {
                acc.push('\\');
            }
            acc.push(character);
            acc
        });
    format!("#\"^{escaped}\"")
}

/// Linux: `bwrap --ro-bind / / … --bind-try <root> <root> … <program> <args…>`。
///
/// root 全体を read-only で束ね、writable root だけを read-write で再 bind する。global config の
/// prefix がある場合は `$HOME` を read-write にし、起動直前に観測した直下 entry のうち prefix family
/// 以外を read-only に戻す。最後に state / launch root を再 bind するため、それらの subtree は writable
/// のまま残る。`--bind-try` / `--ro-bind-try` は列挙後に消えた entry や未作成の state で起動を止めない。
fn linux_argv(
    roots: &[PathBuf],
    prefixes: &[PathBuf],
    home: Option<&Path>,
    home_entries: Option<&[PathBuf]>,
    program: &str,
    program_args: &[String],
) -> Vec<String> {
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
    if !prefixes.is_empty()
        && let (Some(home), Some(home_entries)) = (home, home_entries)
    {
        push_linux_bind(&mut argv, "--bind-try", home);
        for entry in home_entries {
            if !prefixes.iter().any(|prefix| path_has_prefix(entry, prefix)) {
                push_linux_bind(&mut argv, "--ro-bind-try", entry);
            }
        }
    }
    for root in roots {
        push_linux_bind(&mut argv, "--bind-try", root);
    }
    argv.push(program.to_owned());
    argv.extend(program_args.iter().cloned());
    argv
}

fn path_has_prefix(path: &Path, prefix: &Path) -> bool {
    path.to_str()
        .zip(prefix.to_str())
        .is_some_and(|(path, prefix)| path.starts_with(prefix))
}

fn push_linux_bind(argv: &mut Vec<String>, option: &str, root: &Path) {
    let path = root.to_string_lossy().into_owned();
    argv.push(option.to_owned());
    argv.push(path.clone());
    argv.push(path);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(platform: Platform, backend: Option<&str>) -> SandboxRequest {
        SandboxRequest {
            platform,
            mode: SandboxMode::Session,
            protected_root: Some(PathBuf::from("/repo")),
            backend: backend.map(PathBuf::from),
            launch_roots: vec![PathBuf::from("/repo/.usagi/sessions/work")],
            tmpdir: Some(PathBuf::from("/tmp/user")),
            home: Some(PathBuf::from("/home/dev")),
            linux_home_entries: Some(vec![
                PathBuf::from("/home/dev/.claude"),
                PathBuf::from("/home/dev/.claude.json"),
                PathBuf::from("/home/dev/.ssh"),
                PathBuf::from("/home/dev/notes"),
            ]),
            cache_dir: Some(PathBuf::from("/private/var/folders/ab/cd/C")),
            passthrough: false,
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
        let mut request = request(Platform::MacOs, Some("/usr/bin/sandbox-exec"));
        request.mode = SandboxMode::Root;
        request.launch_roots.clear();
        let launched = plan(&request);
        // Launch を into_reject すると None（accessor の Launch 分岐を被覆）。
        assert!(launched.clone().into_reject().is_none());
        let (program, argv) = launched.into_launch().unwrap();
        assert_eq!(program, PathBuf::from("/usr/bin/sandbox-exec"));
        assert_eq!(argv[0], "-p");
        let profile = &argv[1];
        assert!(profile.contains("(deny file-write*)"));
        assert!(profile.contains("mode=root"));
        // Repository-local root は無く、普遍領域だけが subpath になる。
        assert!(!profile.contains("/repo"));
        assert!(profile.contains("(subpath \"/tmp\")"));
        assert!(profile.contains("(subpath \"/home/dev/.claude\")"));
        assert!(profile.contains("(subpath \"/home/dev/Library/Keychains\")"));
        assert!(profile.contains("(subpath \"/Library/Keychains\")"));
        // Keychain 検索が更新する system / per-user の MDS cache。
        assert!(profile.contains("(subpath \"/private/var/db/mds\")"));
        assert!(profile.contains("(subpath \"/private/var/folders/ab/cd/C/mds\")"));
        // macOS firmlink 側（実書き込み先）も許可する。
        assert!(profile.contains("(subpath \"/private/tmp\")"));
        assert!(profile.contains("(subpath \"/private/var/tmp\")"));
        // device node の data 書き込みだけは許す（`/dev/null` が開けないと git すら動かない）。
        // literal 列挙にすると shell の `> /dev/stdout` / `> /dev/fd/1` が拒否されるため、
        // path は subpath のまま動詞だけを絞る、という形をここで固定する。
        assert!(profile.contains("(allow file-write-data (subpath \"/dev\"))"));
        assert!(!profile.contains("(literal \"/dev/null\")"));
        assert!(!profile.contains("file-write-create"));
        // program と引数が profile の後ろに続く。
        assert_eq!(&argv[2..], ["claude", "--print"]);
    }

    #[test]
    fn macos_profile_strips_trailing_slashes_and_adds_private_firmlink_variants() {
        let mut request = request(Platform::MacOs, Some("/usr/bin/sandbox-exec"));
        request.mode = SandboxMode::Root;
        request.launch_roots.clear();
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
    fn a_root_launch_grants_the_state_directory_of_the_agent_it_launches() {
        // 固定の `~/.claude` を配っていた間、root の Codex は自分の state DB へ書けずに
        // 「attempt to write a readonly database」で起動できなかった。grant は exec する
        // program に追従する。
        for (program, state) in [
            ("claude", ".claude"),
            ("codex", ".codex"),
            ("codex-fugu", ".codex-fugu"),
            // PATH 解決済みの絶対 path でも basename で判定する。
            ("/opt/homebrew/bin/codex", ".codex"),
        ] {
            let mut request = request(Platform::MacOs, Some("/usr/bin/sandbox-exec"));
            request.mode = SandboxMode::Root;
            request.launch_roots.clear();
            request.command = vec![program.to_owned()];
            let roots = writable_roots(&request);
            assert!(
                roots.contains(&PathBuf::from(format!("/home/dev/{state}"))),
                "{program} must be able to write ~/{state}"
            );
            // 他 provider の state は貰わない。
            assert_eq!(
                roots
                    .iter()
                    .filter(|root| root.starts_with("/home/dev") && !root.ends_with("Keychains"))
                    .count(),
                1
            );
        }
    }

    #[test]
    fn an_unknown_program_receives_no_home_state_grant() {
        let mut request = request(Platform::Linux, Some("/usr/bin/bwrap"));
        request.mode = SandboxMode::Root;
        request.launch_roots.clear();
        request.command = vec!["/bin/sh".to_owned()];
        assert!(
            !writable_roots(&request)
                .iter()
                .any(|root| root.starts_with("/home/dev"))
        );
        // 判定は closed vocabulary（`DefaultModel`）で、未知 token は None を返す。
        assert_eq!(agent_state_directory("sakana.ai"), Some(".codex-fugu"));
        assert_eq!(agent_state_directory("gemini"), None);
        assert_eq!(agent_state_directory(""), None);
        assert_eq!(agent_state_directory("/"), None);
    }

    #[test]
    fn universal_roots_omit_optional_environment_when_absent() {
        let mut request = request(Platform::Linux, Some("/usr/bin/bwrap"));
        request.mode = SandboxMode::Root;
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
    fn a_macos_root_launch_grants_the_per_user_mds_cache_keychain_search_updates() {
        // per-user の MDS cache が writable でないと Keychain 検索が
        // "A Module Directory Service error has occurred." で失敗し、agent CLI は
        // 古い file 側 credential へ fallback して 401 で起動できない。
        let mut request = request(Platform::MacOs, Some("/usr/bin/sandbox-exec"));
        request.mode = SandboxMode::Root;
        request.launch_roots.clear();
        assert_eq!(
            macos_mds_cache_root(Path::new("/private/var/folders/ab/cd/C")),
            PathBuf::from("/private/var/folders/ab/cd/C/mds")
        );
        assert!(
            writable_roots(&request).contains(&PathBuf::from("/private/var/folders/ab/cd/C/mds"))
        );
        // cache root が無ければ増やさない（他の普遍領域は残る）。
        request.cache_dir = None;
        let without = writable_roots(&request);
        assert!(!without.iter().any(|root| root.ends_with("C/mds")));
        assert!(without.contains(&PathBuf::from("/private/var/db/mds")));
        // Linux の MDS は存在しないため、cache root があっても足さない。
        let mut linux = request.clone();
        linux.platform = Platform::Linux;
        linux.cache_dir = Some(PathBuf::from("/private/var/folders/ab/cd/C"));
        assert!(
            !writable_roots(&linux)
                .iter()
                .any(|root| root.ends_with("mds"))
        );
    }

    #[test]
    fn session_policy_rejects_root_relative_and_workspace_ancestor_write_roots() {
        for root in ["/", "relative", "/repo"] {
            let mut request = request(Platform::Linux, Some("/usr/bin/bwrap"));
            request.launch_roots = vec![PathBuf::from(root)];
            assert!(
                plan(&request)
                    .into_reject()
                    .is_some_and(|reason| reason.contains("writable root")),
                "{root} must be rejected"
            );
        }
        let relative_backend = request(Platform::Linux, Some("relative/bwrap"));
        assert!(
            plan(&relative_backend)
                .into_reject()
                .unwrap()
                .contains("backend")
        );

        let mut root = request(Platform::Linux, Some("/usr/bin/bwrap"));
        root.mode = SandboxMode::Root;
        assert!(
            plan(&root)
                .into_reject()
                .is_some_and(|reason| reason.contains("writable root"))
        );
    }

    // A session launch that only owns its worktree cannot run the agent at all:
    // Claude Code writes its scratchpad to a fixed `/tmp/claude-<uid>/…` that
    // ignores `$TMPDIR`, keeps onboarding / settings / permission mode in
    // `~/.claude`, and reads credentials through the macOS Keychain and MDS
    // cache. Both scopes therefore carry the same universal areas; only the
    // repository write boundary differs.
    #[test]
    fn a_session_launch_carries_the_same_universal_areas_as_a_root_launch() {
        let mut session = request(Platform::MacOs, Some("/sandbox"));
        session.command = vec!["/usr/local/bin/claude".to_owned()];
        let mut root = session.clone();
        root.mode = SandboxMode::Root;
        root.launch_roots.clear();

        let mut expected = writable_roots(&root);
        assert!(expected.contains(&PathBuf::from("/tmp")));
        assert!(expected.contains(&PathBuf::from("/home/dev/.claude")));
        expected.push(PathBuf::from("/repo/.usagi/sessions/work"));
        expected.sort();
        assert_eq!(writable_roots(&session), expected);
    }

    // A session may write the roots its daemon resolved for that exact scope: its
    // checkout and a linked worktree's Git administrative state. Universal areas
    // must not widen that set to the protected workspace itself.
    #[test]
    fn a_session_launch_grants_only_its_resolved_repository_roots() {
        for platform in [Platform::MacOs, Platform::Linux] {
            let mut request = request(platform, Some("/sandbox"));
            request.launch_roots.push(PathBuf::from("/repo/.git"));
            let repository_roots = writable_roots(&request)
                .into_iter()
                .filter(|root| root.starts_with("/repo"))
                .collect::<Vec<_>>();
            assert_eq!(
                repository_roots,
                [
                    PathBuf::from("/repo/.git"),
                    PathBuf::from("/repo/.usagi/sessions/work")
                ]
            );
            assert!(invalid_policy_reason(&request).is_none());
        }
    }

    // Claude keeps onboarding, folder trust and MCP approvals in `~/.claude.json`,
    // which sits **next to** the state directory rather than inside it, and saves
    // it by writing `~/.claude.json.tmp.<pid>.<random>` under `~/.claude.json.lock`
    // and renaming it over the file. Granting only `~/.claude` (or only the exact
    // file) leaves every save denied, so the trust dialog and the first-run flow
    // come back on every launch even though the config reads fine.
    #[test]
    fn a_claude_launch_can_write_the_global_config_file_family_beside_its_state_directory() {
        for mode in [SandboxMode::Session, SandboxMode::Root] {
            let mut request = request(Platform::MacOs, Some("/usr/bin/sandbox-exec"));
            request.mode = mode;
            if mode == SandboxMode::Root {
                request.launch_roots.clear();
            }
            assert_eq!(
                writable_prefixes(&request),
                [PathBuf::from("/home/dev/.claude.json")]
            );
            let (_, argv) = plan(&request).into_launch().unwrap();
            let profile = &argv[1];
            assert!(
                profile.contains(r#"(allow file-write* (regex #"^/home/dev/\.claude\.json"))"#),
                "{profile}"
            );
            // prefix grant は `$HOME` そのものを writable にしない。
            assert!(!profile.contains("(subpath \"/home/dev\")"));
        }
    }

    #[test]
    fn linux_rebinds_home_then_protects_every_non_config_entry() {
        let (_, argv) = plan(&request(Platform::Linux, Some("/usr/bin/bwrap")))
            .into_launch()
            .unwrap();
        assert!(argv.windows(3).any(|window| window[0] == "--bind-try"
            && window[1] == "/home/dev"
            && window[2] == window[1]));
        for protected in ["/home/dev/.ssh", "/home/dev/notes"] {
            assert!(argv.windows(3).any(|window| window[0] == "--ro-bind-try"
                && window[1] == protected
                && window[2] == window[1]));
        }
        assert!(!argv.windows(3).any(|window| window[0] == "--ro-bind-try"
            && window[1] == "/home/dev/.claude.json"));
        assert!(argv.windows(3).any(|window| window[0] == "--bind-try"
            && window[1] == "/home/dev/.claude"
            && window[2] == window[1]));
    }

    #[test]
    fn linux_config_prefix_requires_a_safe_home_entry_inventory() {
        let mut missing = request(Platform::Linux, Some("/usr/bin/bwrap"));
        missing.linux_home_entries = None;
        assert!(
            plan(&missing)
                .into_reject()
                .is_some_and(|reason| reason.contains("inventory"))
        );

        for entry in ["relative", "/home/other/entry", "/home/dev/nested/entry"] {
            let mut invalid = request(Platform::Linux, Some("/usr/bin/bwrap"));
            invalid.linux_home_entries = Some(vec![PathBuf::from(entry)]);
            assert!(
                plan(&invalid)
                    .into_reject()
                    .is_some_and(|reason| reason.contains("直下 path")),
                "{entry} must fail closed"
            );
        }
        #[cfg(unix)]
        {
            use std::ffi::OsString;
            use std::os::unix::ffi::OsStringExt as _;

            let mut non_utf8 = request(Platform::Linux, Some("/usr/bin/bwrap"));
            non_utf8.linux_home_entries = Some(vec![
                PathBuf::from("/home/dev").join(OsString::from_vec(vec![0xff])),
            ]);
            assert!(
                plan(&non_utf8)
                    .into_reject()
                    .is_some_and(|reason| reason.contains("直下 path"))
            );
        }

        let mut root_home = request(Platform::Linux, Some("/usr/bin/bwrap"));
        root_home.home = Some(PathBuf::from("/"));
        root_home.linux_home_entries = Some(Vec::new());
        assert!(
            plan(&root_home)
                .into_reject()
                .is_some_and(|reason| reason.contains("Linux HOME"))
        );

        let mut protected_home = request(Platform::Linux, Some("/usr/bin/bwrap"));
        protected_home.protected_root = Some(PathBuf::from("/home/dev"));
        assert!(
            plan(&protected_home)
                .into_reject()
                .is_some_and(|reason| reason.contains("Linux HOME"))
        );

        // `linux_argv` also fails closed when a caller violates the validated
        // prefix/inventory pairing instead of widening HOME accidentally.
        let argv = linux_argv(
            &[],
            &[PathBuf::from("/home/dev/.claude.json")],
            None,
            None,
            "claude",
            &[],
        );
        assert!(!argv.iter().any(|argument| argument == "--bind-try"));
    }

    #[test]
    fn lexical_config_prefix_cannot_overlap_a_protected_workspace() {
        for mode in [SandboxMode::Session, SandboxMode::Root] {
            let mut request = request(Platform::Linux, Some("/usr/bin/bwrap"));
            request.mode = mode;
            request.protected_root = Some(PathBuf::from("/home/dev/.claude.json-repository"));
            request.launch_roots.clear();
            assert!(
                plan(&request)
                    .into_reject()
                    .is_some_and(|reason| reason.contains("writable prefix"))
            );
        }

        #[cfg(unix)]
        {
            use std::ffi::OsString;
            use std::os::unix::ffi::OsStringExt as _;

            let mut non_utf8_home = request(Platform::MacOs, Some("/usr/bin/sandbox-exec"));
            non_utf8_home.home = Some(PathBuf::from(OsString::from_vec(vec![b'/', 0xff])));
            assert!(
                plan(&non_utf8_home)
                    .into_reject()
                    .is_some_and(|reason| reason.contains("writable prefix"))
            );
        }
    }

    #[test]
    fn the_global_config_grant_follows_the_program_that_is_actually_exec_ed() {
        // config を自分の state directory の中に持つ provider には prefix を配らない。
        assert_eq!(agent_config_prefix("claude"), Some(".claude.json"));
        assert_eq!(
            agent_config_prefix("/usr/local/bin/claude"),
            Some(".claude.json")
        );
        assert_eq!(agent_config_prefix("codex"), None);
        assert_eq!(agent_config_prefix("codex-fugu"), None);
        assert_eq!(agent_config_prefix("gemini"), None);
        assert_eq!(agent_config_prefix(""), None);
        assert_eq!(agent_config_prefix("/"), None);

        let mut codex = request(Platform::MacOs, Some("/usr/bin/sandbox-exec"));
        codex.command = vec!["codex".to_owned()];
        assert!(writable_prefixes(&codex).is_empty());
        let (_, argv) = plan(&codex).into_launch().unwrap();
        assert!(!argv[1].contains("regex"));

        // `$HOME` が無ければ prefix も無い。
        let mut homeless = request(Platform::MacOs, Some("/usr/bin/sandbox-exec"));
        homeless.home = None;
        assert!(writable_prefixes(&homeless).is_empty());
    }

    #[test]
    fn profile_regex_prefixes_escape_every_non_path_character() {
        // `.` を素で通すと任意 1 文字、`"` を素で通すと literal が閉じてしまう。
        assert_eq!(
            sandbox_regex_prefix_literal(Path::new(r#"/a b/.c"d-e_1"#)),
            r#"#"^/a\ b/\.c\"d-e_1""#
        );
    }

    #[test]
    fn profile_string_literals_escape_quotes_and_backslashes() {
        let literal = sandbox_string_literal(Path::new(r#"/a"b\c"#));
        assert_eq!(literal, r#""/a\"b\\c""#);
    }

    #[test]
    fn the_test_seam_is_off_unless_a_debug_build_sees_the_exact_opt_in_value() {
        assert!(passthrough_requested(true, Some("1")));
        // release ビルド（debug_assertions なし）では環境変数があっても拘束を外さない。
        assert!(!passthrough_requested(false, Some("1")));
        // 不在・空・別値はいずれも opt-in にならない。
        assert!(!passthrough_requested(true, None));
        assert!(!passthrough_requested(true, Some("")));
        assert!(!passthrough_requested(true, Some("true")));
        assert_eq!(
            PASSTHROUGH_ENVIRONMENT_VARIABLE,
            "USAGI_CLAUDE_SANDBOX_PASSTHROUGH"
        );
    }

    #[test]
    fn passthrough_execs_the_command_itself_without_requiring_a_backend() {
        // backend も未対応 platform も要求しない（bwrap の無い Linux CI 用の seam）。
        let mut request = request(Platform::Unsupported, None);
        request.passthrough = true;
        let (program, argv) = plan(&request).into_launch().unwrap();
        assert_eq!(program, PathBuf::from("claude"));
        assert_eq!(argv, ["--print"]);
        // 空 command は passthrough でも fail-closed のまま。
        request.command.clear();
        assert!(plan(&request).into_reject().is_some());
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
