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
[![Rust](https://img.shields.io/badge/Rust-2024-orange.svg?logo=rust&logoColor=white)](https://rust-lang.org/)

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
[TUI 仕様](document/03-tui.md)、全キーボード操作は
[キーバインド](document/11-keybindings.md)を参照する。

## 画面

`usagi` を起動すると Welcome 画面を表示する。登録済み workspace を **Open** から選ぶほか、
最近使った workspace を **Recent** から直接開き、**New** で既存リポジトリの登録または clone、
**Config** で全体設定の編集ができる。

Home の compact な表示は Nerd Font アイコンが既定である。patched font を使えない端末では
Global Config の `Icons` を `text` にすると、PR・Agents・CPU / memory・通知・session cursor などを
`PR` / `Agents` / `CPU` / `MEM` / `!` / `>` の文字表示へ切り替えられる。

workspace を開くと Home へ移る。最上段の project tab bar には同じ TUI で開いている workspace が並び、
選択中 workspace の session と Preview / Terminal / Diff / Notes をその下へ全面表示する。project を再選択すると、
その project で最後にフォーカスしていた session へ Switch のカーソルが戻る。`+ Open` は左右の余白を
含めてクリックでき、登録済み workspace の複数選択に加えて `Tab` から既存ディレクトリを直接追加できる。overlay を
開いている間は別の usagi が追加した workspace も自動で一覧へ反映される。Session Garden では開いている全 project の
session を区画として、観測済みの全 Agent を 1 runtime 1 うさぎで表示する。Garden は pending decision、失敗、
waiting / interrupted Agent のある区画を Action Center として集約する。端末の広さと件数に合わせて大きな区画から
Agent ごとの card / glyph まで密度を自動調整し、横移動なしで 1 画面へ収める。うさぎをクリックすると、その project の
Closeup と該当 Agent tab へ移動する。workspace root の shell は header の
`[ ⌂ Shell ]` から、下端より重なる専用 drawer として開く。

```text
 1 usagi   2 api   3 web   + Open
┌─ sessions ───────────┬─ Preview / Terminal / Diff / Notes ──────┐
│   feature-login      │                                          │
│   12m ago  #42  +18  │  Session info, terminal, and diff        │
│ > review-auth        │                                          │
│   now      ↑1   +4   │  Agent and shell run in daemon PTYs;     │
│   + new session      │  the TUI attaches here                   │
└──────────────────────┴──────────────────────────────────────────┘
```

Home では画面上のヒントか `?`（live pane では leader に続けて `?`）で、その場で有効な操作を確認できる。
session がない workspace を開いた直後は session 行を選択しないため、画面上の `+ new session` を選ぶか、
ヘルプに表示される作成操作を使う。キーボード入力の完全な一覧と leader の規則は
[キーバインド](document/11-keybindings.md)だけを正本とし、README には複製しない。

live terminal にフォーカスがある間は、Director の root Agent を含めて leader 以外の入力を PTY へ直接渡す。
TUI を離れる操作は
daemon-owned process を停止せず、接続だけを外す。正確な入力所有権と終了時の挙動は
[workspace の離脱と終了](document/03-tui.md#workspace-の離脱と終了)が正本である。

Overview で `daemon` を実行すると、health・resource 使用量・session / Agent 状態をまとめた管理 modal が開く。
ここから non-force の Start / Restart / Stop を実行でき、live runtime を破棄し得る強制操作は CLI にだけ残る。
詳細は [Overview と modal](document/03-tui.md#overview-と-modal)を参照する。

generic terminal の `Ctrl-C` は foreground command を割り込んで画面をクリアし、prompt を先頭へ戻す。
`Ctrl-O x` / `Ctrl-O Ctrl-X` は shell を終了するため、再度開くと新しい terminal になる。Director は画面の
右側を高さ一杯に使い、workspace Shell と同じ drawer layer で横に並べて開ける。両方が開いているときは Shell の横幅を
Director の左端までに縮め、panel のクリックでも focus が移る。左側の Shell では選択・コピーを継続でき、
title の `FOCUS` / `click to focus` が入力先を示す。選択 session の Agent は
drawer の背面でも通常の workspace geometry と attachment のまま出力を更新し続ける。最後の実行中または起動中 root Agent が
消えても Director と現在 route は保持され、Console は停止状態を表示する。
live Agent では同じ close chord が `Ctrl-D` と同じ終了入力になり、interrupted Agent では選択中の tab を
永続的に閉じる。interrupted tab の close は Agent の resume や新規起動を行わない。
interrupted tab をクリックまたは tab 移動で明示選択すると、exact resume 可能ならその会話だけを再開する。
resume 不可なら Remove / Keep の確認を出し、Remove を確定した lineage だけを永続的に一覧から除く。

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
最新版へ、`usagi update -v` で選んだ release へ更新できる。更新後の CLI は次回起動から使われる。更新前に
installer は更新 lock を保持したまま exact installed binary の内部 Doctor を必ず実行し、同期時点の daemon 状態を
lifecycle lock の下で再確認する。daemon が動いていれば安全な handoff と新 daemon の応答確認が完了するまで待ち、
停止中または crash 後の stale owner だけなら回収して停止状態を維持する。
起動中の TUI は終了して開き直す。

live Agent の process-local MCP credential を新 daemon へ安全に渡せない場合は、binary は更新するが daemon handoff を拒否して
update を非 0 で終え、Agent と旧 daemon を維持する。Agent を終了した後に `usagi doctor --fix` を再実行する。
選択した旧 release が安全な managed 同期 capability を持たない場合や、稼働中の旧 daemon が server-side handoff fence を
証明できない場合も旧 daemon を変更せず拒否する。後者は旧 daemon を停止してから更新を再実行する。

この managed 同期を含まない旧版から初めて更新する 1 回だけは、実行中の旧 `update` 自体を遡及変更できないため binary の
差し替えだけで終わる。その場合は新しい `usagi update` または `usagi doctor --fix` をもう一度実行する。

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
から選べる。Home の `+ Open` では `Tab` を押して既存ディレクトリのパスを入力しても登録・open できる。
新しいリポジトリを clone したい場合は Welcome の New を使う。

### 2. session を作る

Home の `+ new session` を選んで名前を入力するか、コマンドパレットで作成する。

```text
session create feature-login
```

CLI から daemon へ直接依頼することもできる。

```bash
usagi session create feature-login
usagi session create remote-fix --base refs/remotes/origin/main
# Configでhierarchicalまたはflat Teamを選択済みの場合
usagi session create implementation --role worker
```

session は対象リポジトリの `.usagi/sessions/<name>/` に独立した worktree として作られる。
Home の作成欄では `local:main` / `remote:origin/(default)` / `remote:origin/main` のように
出所を区別した base branch を `↑↓` で選ぶ。`(default)` はその remote の既定 branch を表す。
CLI の `--base` は同じ対象を fully-qualified ref で指定する。
role は有効なTeamまたは`roles.toml`から作業種別ごとの追加指示を選ぶ stable ID で、権限や sandbox を変更するものではない。
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
terminal new     # 外部ターミナルを開き、modal を閉じて Closeup へ戻る
```

daemon 再起動などで Agent が中断した場合は、自動的に別の会話へ接続せず、保持された provider conversation を
TUI の interrupted tab 選択、または `session resume <name>` で明示的に再開する。TUI の起動・workspace open・
inventory refresh 自体は再開を発火しない。
終了済みの会話を保持したまま Agent process と PTY だけを止める場合は `session sleep <name>` を使う。
同時起動枠は 16 で、枠が埋まった状態から新しく起動すると、exact resume 可能な終了済み Agent のうち最古の 1 件が
自動的に sleep へ移る。session、worktree、provider conversation は削除されず、同じ `session resume <name>` で再開できる。

### 4. 状態と PR を確認する

session の 2 行目には最終利用時刻、base branch との差分、右端に PR アイコンと件数を表示する。
`Icons: text` ではアイコンを `PR` label へ縮退する。`Ctrl-O p`
（または `Ctrl-O Ctrl-P`）、または右端の PR 表示のクリックは、PR がある場合だけ一覧を開く。
File Preview は `Ctrl-O v` で開く。選択中 session の worktree（workspace root target では workspace root）から
tracked file と gitignore 対象外の未追跡 file を fuzzy 検索し、`Enter` で UTF-8 text を読み取り専用表示する。
本文の `Esc` は file 一覧へ戻り、一覧の `Esc` は Preview を閉じる。scratchpad は `Ctrl-O s` で開く。
起動後に新しい PR を検知すると、別のモーダルを操作中でなければ
検知した PR を選択した一覧を自動で開く。PR 一覧は repository 見出しの下へ番号・状態・title をまとめ、
上部の All / Open / Closed / Merged を `←→`、PR を `↑↓` で選ぶ。枠外のクリックで閉じ、PR を選んで
Enter を押すと既定のブラウザで開く。

PR がすべて merged になり Agent が作業中でない session は、Overview の
`session cleanup` で安全な cleanup queue にまとめられる。Space（`a` で全件）で選び、Enter を押すと daemon の
完了 snapshot を1件ずつ確認しながら順番に削除する。dirty worktree や削除不能な branch は daemon が拒否し、queue は
そこで停止する。

## AI エージェントとの連携

daemon から起動した Agent には usagi の stdio MCP server が組み込まれる。Agent は作業中の session から、
次のような操作を行える。MCP child の cwd が provider によって変わっても、issue の保存先は daemon が認証した
その session の worktree、memory の保存先は Git 追跡外にある workspace 専用の daemon data home に固定される。

| 系統 | 用途 |
|---|---|
| `session_*` | session の作成・削除・状態確認、prompt 配送、別 Agent への委譲 |
| `issue_*` | git で共有する `.usagi/issues/` のタスクを検索・更新する |
| `memory_*` | 同じ workspace の root/session Agent で共有する Git 追跡外の知識を保存・検索する |
| `agent_*` | 委譲した worker の完了報告と inbox を扱う |
| `terminal_*` | 同じ session/worktree にある通常 terminal の出力を read-only で確認する |
| `user_decision_*` | Agent から利用者へ判断を依頼し、TUI で回答する |
| `supervisor_*` | 複数 step の durable な実行・再試行・確認を管理する |

issue / memory tool は workspace 設定で無効化できる。MCP の公開 tool、認証、daemon への反映経路は
[MCP サーバ仕様](document/07-mcp.md)が正本である。

## 設定

Welcome の Config は全体設定、workspace のコマンドパレットにある `config` はその workspace の設定を編集する。

| 設定 | 内容 |
|---|---|
| Theme / Icons / Modal mode / PR auto-open | TUI の配色、Nerd Font または text の表示、Overview / Closeup の操作方式、PR検知時の表示方法 |
| [Terminal PTYs](document/05-daemon.md#capacity-pool) | generic Terminal の同時 PTY 数に対する最後の安全上限。画面メモリは保持中の実セル数で別に制御する |
| Agent | 新しい Agent pane の既定 CLI |
| Base branch | workspace で新しい session を作るときの既定 branch |
| Workflow | 択一の `< classic >`（既定）または `< goal-driven >`。前者は New Conversation、後者は Start Work Run を開き、Goal Composer で目的を一度入力して review-ready PR または明示判断まで継続する固定指示と Goal を root Agent へ渡す。Work Run の委譲先は root と同じ provider/runtime を使い、task に必要十分な model を選ぶ |
| [Team](document/10-session-roles.md#catalog) | Enterで構造図付きカードを開き、`none` / `hierarchical`（階層型）/ `flat`（フラット）/ `pipeline`（パイプライン）から session role 構造を選択 |
| Issue / Memory | 対応する MCP tool 群の公開可否 |
| Environment | global と workspace の 2 層で、次回起動する pane へ渡す環境変数 |
| [Roles](document/10-session-roles.md#rolestoml-の設定例) | `roles [workspace|global]` で編集する session / root ごとの追加 instruction、既定 role、委譲制限 |

`goal-driven` を選んだ workspace では `Ctrl-O n` または Director の `[ Start ]` から Goal Composer を開き、目的と
provider を確定して Work Run を開始する。Director の初回表示は `goal-driven` では Work Runs、`classic` では
Organization になり、同じ Workflow で閉じて開き直した場合は直前の画面へ戻る。Classic の Organization は Conversation 一覧と
選択 Conversation の Agent / Session tree を表示し、goal-driven からは開かない。goal-driven の `Ctrl-O w` は最大16件の Work Runs を
直接開き、`Enter` で選択 Run の Run Overview へ進む。Work Runs / Run Overview の `Ctrl-C` は active Run の cancel 確認、
`Ctrl-X` は終了済み Run の履歴削除確認で、確認中の `Ctrl-C` / `Esc` は取り消しになる。Workflow を切り替えても daemon 上の
Conversation / Work Run は継続するが、Director は選択中 Workflow の画面 tree だけを開く。一般の blocking choice は既存 Decision、
成果 PR は既存 PR 一覧に表示する。
詳細な操作と現行契約は [goal-driven workflow](document/03-tui.md#goal-driven-workflow)を参照する。

環境変数は Config の `Env  [ N variables ]`、Overview の `env [workspace|global]`、Closeup の `env` で編集する。
Workspace Config と Closeup は同じ複数行 editor で
workspace の値だけを変更し、global の値は変更しない。同名の値は workspace 側が優先され、
`op://vault/item/field` は 1Password CLI で解決してから子プロセスへ渡される。secret 本体は設定ファイルに保存しない。
保存場所、解決順序、予約変数は [環境変数設定](document/09-env.md)を参照する。

## CLI

ここでは日常的に使う入口をコマンド系統ごとに示す。公開 CLI の完全な command tree と各 verb の役割は
[プロジェクト概要#入口面](document/01-overview.md#入口面)を正本とし、オプションの最終的な構文は
`usagi <command> --help` で確認する。

| コマンド | 用途 |
|---|---|
| `usagi` / `usagi hop` | Welcome TUI を開く |
| `usagi open [path]` | workspace を登録して直接開く |
| `usagi config` | Global Config を開く |
| `usagi doctor` | 必要ツールの診断画面を開く |
| `usagi doctor --fix` | client / daemon build と Agent の hook・MCP integration revision を診断し、live MCP authority を失わない場合だけ古い daemon を seamless restart する |
| `usagi doctor --fix --restart-agents` | 古い integration の Agent を一覧化・停止し、provider session ID を使って現在の設定で再開する。Running の Agent は拒否する |
| `usagi doctor --fix --restart-agents --force` | Running（tool / prompt 実行中）の Agent も明示的に中断して再開する |
| `usagi clean [--dry-run\|--apply [--force]]` | 紐付いていない workspace・daemon data・worktree・branch と、消滅した generation が握ったままの capacity claim を検出・削除する |
| `usagi update` / `usagi update -v` | 最新版、または選択した公開 release のバイナリへ更新し、稼働していた daemon を更新済み binary の Doctor で同期する |
| `usagi completion <shell>` | shell 補完を生成する |
| `usagi version` / `usagi --version` | version を表示する |
| `usagi session <command>` | daemon-owned session の作成・削除・sleep・resume・setup・prompt を操作する |
| `usagi daemon <command>` | daemon の起動・状態確認・tenant解放・停止・入替・service登録を操作する |

`usagi clean` は dry-run で候補だけを表示する。`--apply` は欠損 path の workspace 登録、欠損 workspace の
daemon data、lifecycle に存在しない `usagi/*` branch と `.usagi/sessions/*` worktree を削除する。dirty worktree と
未マージ branch は `--apply --force` を明示した場合だけ削除し、daemon が使用中の workspace はスキップする。
Open、Recent、Unite で元ディレクトリが消えた workspace を選ぶと、その場で登録解除の確認を表示する。
`Remove` が削除するのは workspace registry entry だけで、daemon data は `usagi clean --apply` の対象として残す。

加えて、消滅した daemon generation が握ったままの capacity claim も回収する。claim は Agent と terminal の
固定サイズ pool を占有するため、取りこぼしが積み上がると pool が枯渇して**どの session でも Agent を起動できなくなる**。
回収対象は「どの owner shard も資源を説明せず、かつ generation registry が owner を載せていない」claim だけで、
どちらか一方しか満たさない claim は通常の durable state として残す（正本は
[owner-generation runtime shard と global resource allocator](document/05-daemon.md#owner-generation-runtime-shard-と-global-resource-allocator)）。

`restart` は live runtime が無ければ cold transition、あれば通常は PTY を維持する seamless rollover を行い、
安全な handoff の前提が欠ける場合だけ拒否する。`stop` は live Agent や terminal があると拒否する。
`--force` は live PTY を破棄してよい場合だけ使う。通常の planned replacement も daemon-provisioned MCP credential を持つ
live Agent がいれば旧 owner を維持して拒否する。詳細は [planned replacement](document/05-daemon.md#planned-replacement)を参照する。
service 登録は macOS（LaunchAgent）と Linux（systemd user unit。systemd 240 以降）で利用できる。
登録した service は、登録時に解決した workspace を起動 directory として固定する。login 時の起動と異常終了後の
復帰を担うが、`usagi daemon stop` による意図した停止の扱いは異なる（LaunchAgent は起動し直し、systemd unit は
停止のまま残す）。Linux でログアウトをまたいで常駐させるには `loginctl enable-linger` が別途必要である。
詳細は [service supervision](document/05-daemon.md#service-supervision) を参照する。
登録しない場合も、TUI・`usagi mcp`・`usagi session ...` の接続時に daemon は自動起動し、まだ開いていない
リポジトリも、そのリポジトリの root で実行すればその接続で開く（先に TUI で開いておく必要はない）。全コマンドの現在の動作は
[入口面の一覧](document/01-overview.md#入口面)を参照する。

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
