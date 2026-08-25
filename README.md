# usagi

<div align="center">

<pre>
    (\(\&#160;&#160;&#160;&#160;&#160;&#160;&#160;&#160;&#160;&#160;&#160;&#160;&#160;&#160;&#160;&#160;&#160;&#160;&#160;&#160;&#160;&#160;&#160;
   (='-')     ╻ ╻ ┏━┓ ┏━┓ ┏━╸ ╻
  o(_(")(")   ┃ ┃ ┗━┓ ┣━┫ ┃╺┓ ┃
              ┗━┛ ┗━┛ ╹ ╹ ┗━┛ ╹
</pre>

**AI エージェントの並列開発を、セッション・worktree・端末ごと束ねる TUI / CLI**

複数の AI エージェントを隔離された worktree で動かし、作業の委譲から PR の確認までを
ひとつの画面で進める。

[![Test](https://github.com/KKyosuke/usagi/actions/workflows/test.yml/badge.svg)](https://github.com/KKyosuke/usagi/actions/workflows/test.yml)
[![Coverage](https://github.com/KKyosuke/usagi/actions/workflows/coverage.yml/badge.svg)](https://github.com/KKyosuke/usagi/actions/workflows/coverage.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-2024-orange.svg?logo=rust&logoColor=white)](https://www.rust-lang.org/)

</div>

> この README は現在の `usagi` を説明する。GitHub Releases とリポジトリルートは
> 同じ実装を提供する。

## usagi でやりたいこと

usagi が目指すのは、複数種類の AI エージェントを同じ UI から操作し、並行する作業を session として
管理できる開発環境である。

- Claude、Codex、Sakana AI と通常の terminal をひとつの画面で起動し、切り替えながら操作する。
- 作業ごとに session を作り、独立した git worktree、Agent、terminal、差分、PR、ノートをまとめて管理する。
- TUI を離れても作業を daemon 上で継続し、あとから同じ session に戻る。
- issue や memory を Agent と共有し、ひとつの作業を別の session へ委譲しながら進める。

設計上の位置づけと現在の実装範囲は
[プロジェクト概要](document/01-overview.md)、画面とキー操作の詳細は
[TUI 仕様](document/03-tui.md)を参照する。

## 画面

`usagi` を起動すると Welcome 画面を表示する。登録済み workspace を **Open** から選ぶほか、
最近使った workspace を **Recent** から直接開き、**New** で既存リポジトリの登録または clone、
**Config** で全体設定の編集ができる。

workspace を開くと Home へ移り、左側に session、右側に選択した session の Preview / Terminal /
Diff / Notes と live pane を表示する。

```text
┌─ sessions ───────────┬─ Preview / Terminal / Diff / Notes ──────┐
│   feature-login      │                                          │
│   12m ago  #42  +18  │  Session info, terminal, and diff        │
│ > review-auth        │                                          │
│   now      ↑1   +4   │  Agent and shell run in daemon PTYs;     │
│   + new session      │  the TUI attaches here                   │
└──────────────────────┴──────────────────────────────────────────┘
```

Home の基本操作は次のとおり。

| 操作 | 動作 |
|---|---|
| `↑` / `↓`、`j` / `k` | session を選ぶ |
| `←` / `→`、`h` / `l` | Preview / Terminal / Diff / Notes を切り替える |
| `Enter` / `t` | 選択した session の Closeup を開く |
| `Ctrl-O` | live pane から Switch へ戻る、または Closeup の action を開く |
| `:` | Overview のコマンドパレットを開く |
| `p` / `v` / `d` / `n` | PR / preview / diff / notes を開く |
| `Ctrl-Q` | workspace を離れるか、TUI を終了するか選ぶ |

live terminal にフォーカスがある間は、`Ctrl-O` prefix 以外の入力を PTY へ渡す。TUI を離れる操作は
daemon-owned process を停止せず、接続だけを外す。正確な入力所有権と終了時の挙動は
[workspace の離脱と終了](document/03-tui.md#workspace-の離脱と終了)が正本である。

## 必要なもの

- Rust / Cargo（`rust-toolchain.toml` で必要な nightly toolchain を固定）
- Git
- 起動する Agent の CLI（必要なものだけ）
  - Claude: `claude`
  - OpenAI Codex: `codex`
  - Sakana AI: `codex-fugu`

v2 の daemon IPC と PTY 管理は Unix transport を使うため、現行の主要な実行対象は macOS / Linux である。

## インストール

### installer で導入する

installer は最新の公開 release を `~/.usagi/bin/usagi` へ導入する。archive の SHA-256 と release version
artifact を検証してから差し替える。

```bash
curl -fsSL https://raw.githubusercontent.com/KKyosuke/usagi/main/scripts/install.sh | bash
```

`~/.usagi/bin` が `PATH` に無い場合は installer が追記方法を案内する。導入済みなら `usagi update` で
最新版へ、`usagi update -v` で選んだ release へ更新できる（反映には再起動が必要）。

対象は macOS（amd64 / arm64）と Linux（amd64）である。v2 の daemon IPC と PTY 管理は Unix transport を
使うため Windows は対象外で、installer もこの 3 つ以外は失敗する。

### ソースからビルドする

```bash
git clone https://github.com/KKyosuke/usagi.git
cd usagi
cargo build --release
```

生成されるバイナリは `target/release/usagi` にある。Cargo の bin directory へ導入する場合は次を使う。

```bash
cargo install --path . --locked
```

> ソースからビルドしたバイナリは、`USAGI_RUNTIME_MODE` を指定しなければ状態を `~/.usagi/local/` に
> 置く（開発中の実行が本番の状態を触らないようにするため）。公開 release の artifact は
> `~/.usagi` 自体を使う。詳細は [artifact の既定 mode](document/05-daemon.md#artifact-の既定-mode) を参照する。

### Tab 補完

`usagi completion <shell>` は、CLI 定義から補完スクリプトを標準出力へ生成する。

```bash
source <(usagi completion bash)
usagi completion zsh > ~/.zfunc/_usagi
usagi completion fish > ~/.config/fish/completions/usagi.fish
```

## Quick Start

### 1. workspace を開く

対象のリポジトリを登録して直接開く。

```bash
usagi open /path/to/project
```

引数を省略するとカレントディレクトリを開く。次回からは `usagi` の Welcome にある Open / Recent
から選べる。新しいリポジトリを clone したい場合は Welcome の New を使う。

### 2. session を作る

Home の `+ new session` を選んで名前を入力するか、コマンドパレットで作成する。

```text
session create feature-login
```

CLI から daemon へ直接依頼することもできる。

```bash
usagi session create feature-login
usagi session create review-auth --role reviewer
```

session は対象リポジトリの `.usagi/sessions/<name>/` に独立した worktree として作られる。
role は作業種別ごとの追加指示を選ぶ stable ID で、権限や sandbox を変更するものではない。
詳細は [session role](document/10-session-roles.md)を参照する。

### 3. Agent または terminal を開く

session を選んで Closeup に入り、`agent` または `terminal` を実行する。新しい pane は daemon が所有し、
TUI は live output を表示して入力を転送する。TUI を終了しても process は daemon 上で継続する。

Agent の選択例:

```text
agent             # workspace の既定 Agent
agent -m claude
agent -m codex
agent -m sakana.ai
terminal
```

daemon 再起動などで Agent が中断した場合は、自動的に別の会話へ接続せず、保持された provider conversation を
`session resume <name>` で明示的に再開する。

### 4. 状態と PR を確認する

session の 2 行目には最終利用時刻、base branch との差分、右端に PR アイコンと件数を表示する。Switch の `p`、
Closeup の `Ctrl-O Ctrl-P`、または右端の PR 表示のクリックで PR 一覧を開き、`d` で diff、
`n` で session の scratchpad を開く。起動後に新しい PR を検知すると、別のモーダルを操作中でなければ
検知した PR を選択した一覧を自動で開く。PR を選んで Enter を押すと既定のブラウザで開く。

## AI エージェントとの連携

daemon から起動した Agent には usagi の stdio MCP server が組み込まれる。Agent は作業中の session から、
次のような操作を行える。

| 系統 | 用途 |
|---|---|
| `session_*` | session の作成・削除・状態確認、prompt 配送、別 Agent への委譲 |
| `issue_*` | git で共有する `.usagi/issues/` のタスクを検索・更新する |
| `memory_*` | git で共有する `.usagi/memory/` の知識を保存・検索する |
| `agent_*` | 委譲した worker の完了報告と inbox を扱う |
| `user_decision_*` | Agent から利用者へ判断を依頼し、TUI で回答する |
| `supervisor_*` | 複数 step の durable な実行・再試行・確認を管理する |

issue / memory tool は workspace 設定で無効化できる。MCP の公開 tool、認証、daemon への反映経路は
[MCP サーバ仕様](document/07-mcp.md)が正本である。

## 設定

Welcome の Config は全体設定、workspace のコマンドパレットにある `config` はその workspace の設定を編集する。

| 設定 | 内容 |
|---|---|
| Theme / Modal mode / PR auto-open | TUI の配色、Overview / Closeup の操作方式、PR検知時の表示方法 |
| Agent | 新しい Agent pane の既定 CLI |
| Team | Enterで構造図付きカードを開き、`none` / 階層型 / フラット / パイプラインから session role 構造を選択 |
| Issue / Memory | 対応する MCP tool 群の公開可否 |
| Environment | global と workspace の 2 層で、次回起動する pane へ渡す環境変数 |
| Roles | session / root ごとの追加 instruction と既定 role |

環境変数は Config の `Env  [ N variables ]`、Overview の `env [workspace|global]`、Closeup の `env` で編集する。
Workspace Config と Closeup は同じ複数行 editor で
workspace の値だけを変更し、global の値は変更しない。同名の値は workspace 側が優先され、
`op://vault/item/field` は 1Password CLI で解決してから子プロセスへ渡される。secret 本体は設定ファイルに保存しない。
保存場所、解決順序、予約変数は [環境変数設定](document/09-env.md)を参照する。

## CLI

| コマンド | 用途 |
|---|---|
| `usagi` / `usagi hop` | Welcome TUI を開く |
| `usagi open [path]` | workspace を登録して直接開く |
| `usagi config` | Global Config を開く |
| `usagi doctor` | 必要ツールの診断画面を開く |
| `usagi update` / `usagi update -v` | 最新版、または選択した公開 release のバイナリへ更新する |
| `usagi completion <shell>` | shell 補完を生成する |
| `usagi version` / `usagi --version` | version を表示する |
| `usagi session ...` | daemon-owned session を作成・削除・resume する |
| `usagi daemon start\|status\|stop\|restart` | daemon lifecycle を操作する |
| `usagi daemon install-service` | daemon を OS の service として登録する（macOS は LaunchAgent、Linux は systemd user unit） |

`restart` は live runtime が無ければ cold transition、あれば通常は PTY を維持する seamless rollover を行い、
安全な handoff の前提が欠ける場合だけ拒否する。`stop` は live Agent や terminal があると拒否する。
`--force` は live PTY を破棄してよい場合だけ使う。詳細は [planned replacement](document/05-daemon.md#planned-replacement)を参照する。
service 登録は macOS（LaunchAgent）と Linux（systemd user unit。systemd 240 以降）で利用できる。
登録した service は、登録時に解決した workspace を起動 directory として固定する。login 時の起動と異常終了後の
復帰を担うが、`usagi daemon stop` による意図した停止の扱いは異なる（LaunchAgent は起動し直し、systemd unit は
停止のまま残す）。Linux でログアウトをまたいで常駐させるには `loginctl enable-linger` が別途必要である。
詳細は [service supervision](document/05-daemon.md#service-supervision) を参照する。
登録しない場合も、TUI・`usagi mcp`・`usagi session ...` の接続時に daemon は自動起動し、まだ開いていない
リポジトリも、そのリポジトリの root で実行すればその接続で開く（先に TUI で開いておく必要はない）。全コマンドの現在の動作は
[実装状態の一覧](document/01-overview.md#現在の実装状態)を参照する。

## アーキテクチャ

v2 は TUI、daemon、CLI / MCP、共通ロジックを分離した 4 クレートと、実 IO を束ねる合成ルートで構成する。

```text
.
├── Cargo.toml          # workspace と配布バイナリ usagi
├── src/                # 合成ルート: process / terminal / OS IO
└── crates/
    ├── core/           # domain / usecase / 共有 infrastructure
    ├── cli/            # CLI と stdio MCP server
    ├── daemon/         # session・Agent・PTY の authority
    └── tui/            # 純粋な TUI state・描画・入力処理
```

実行面同士は直接依存せず `usagi-core` の型と port を介する。依存方向、各クレートの責務、process argv
contract は [アーキテクチャ](document/02-architecture.md)が正本である。

## Development

toolchain は `rust-toolchain.toml` に固定されている。リポジトリルートで実行する。

| 目的 | コマンド |
|---|---|
| ビルド確認 | `cargo check --workspace --all-targets` |
| フォーマット確認 | `cargo fmt --all -- --check` |
| Lint | `cargo clippy --workspace --all-targets -- -D warnings` |
| テスト | `cargo test --workspace --quiet` |
| 実行 | `cargo run -- [args]` |

変更中・commit 前・CI で必要な gate は異なる。coverage 100% を含む品質基準、ブランチ、コミット、PR、
リリースの規約は [開発規約](document/06-conventions.md)を正本とする。仕様ドキュメント全体は
[document/README.md](document/README.md)から参照できる。

## License

[MIT](LICENSE)
