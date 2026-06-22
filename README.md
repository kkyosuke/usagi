<div align="center">

<pre>
   (\(\
  (='-')      ╻ ╻ ┏━┓ ┏━┓ ┏━╸ ╻
  o(_(")(")   ┃ ┃ ┗━┓ ┣━┫ ┃╺┓ ┃
              ┗━┛ ┗━┛ ╹ ╹ ┗━┛ ╹
</pre>

# usagi 🐰

**AI Agent のワークフローを管理する TUI / CLI ツール**

複数の AI エージェントを worktree ごとに走らせ、セッション・タスク・メモリを 1 画面で束ねる。

<br>

[![Test](https://github.com/KKyosuke/usagi/actions/workflows/test.yml/badge.svg)](https://github.com/KKyosuke/usagi/actions/workflows/test.yml)
[![Coverage](https://github.com/KKyosuke/usagi/actions/workflows/coverage.yml/badge.svg)](https://github.com/KKyosuke/usagi/actions/workflows/coverage.yml)
[![Release](https://github.com/KKyosuke/usagi/actions/workflows/release.yml/badge.svg)](https://github.com/KKyosuke/usagi/actions/workflows/release.yml)
<br>
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-2021-orange.svg?logo=rust&logoColor=white)](https://www.rust-lang.org/)
[![Platform](https://img.shields.io/badge/platform-macOS%20%7C%20Linux%20%7C%20Windows-blue.svg)](#installation)

</div>

> [usagi.ai](https://github.com/KKyosuke/usagi.ai) の設計を引き継いだ再構築プロジェクトです。

## 起動画面

`usagi hop` を実行すると、うさぎのマスコットと `USAGI` タイトルがフェードインするスプラッシュから始まり、ワークスペースを開くとホーム画面へ遷移します。

<table>
<tr>
<td>

**スプラッシュ → ウェルカム**

```text
            (\(\
           (='-')
           o(_(")(")

            U S A G I
```

</td>
<td>

**ホーム画面（統括モード）**

```text
        usagi · ▸ root · 4 sessions
   Overview › Switch › Focus › Attached
 ▎ ⌂   root              │
 ▎     workspace root    │
       ──────────────    │
   ●   main       pushed │   (右ペインは
     ▶ running           │    モードで変化)
   ○   feat/login  local │
     ◆ waiting           │
```

</td>
</tr>
</table>

左ペインは各セッションを 2 行で表示し、稼働中は **`▶ running`（緑）**／入力待ちは **`◆ waiting`（黄）**／アイドルは **`⏸ idle`（シアン）** でひと目で状態がわかります。`Ctrl-O` で一段ズームアウト、`Esc` で一段戻り、`Ctrl+C` で終了します。画面・モード・キー操作の詳細は [document/design/05-home.md](document/design/05-home.md) を参照してください。

## Prerequisites

- Rust (Cargo)
- Git

## Installation

### One-liner (macOS / Linux)

ビルド済みバイナリを 1 行でダウンロードしてインストールできます:

```bash
curl -fsSL https://raw.githubusercontent.com/KKyosuke/usagi/main/scripts/install.sh | bash
```

`~/.usagi/bin` にインストールされます。表示される案内に従って PATH を通してください:

```bash
export PATH="$PATH:$HOME/.usagi/bin"
```

同じコマンドを再実行すると、常に最新リリースを取得して既存のバイナリを置き換えます（アップデート）。インストール後はバージョンの変化に応じて「新規インストール / アップデート / 再インストール」を判別したメッセージが表示されます。

プラットフォームを指定してアーカイブから直接インストールすることもできます:

#### macOS (Apple Silicon)
```bash
curl -L https://github.com/KKyosuke/usagi/releases/latest/download/usagi-macos-arm64.tar.gz | tar -xz && ./install.sh && rm install.sh
```

#### macOS (Intel)
```bash
curl -L https://github.com/KKyosuke/usagi/releases/latest/download/usagi-macos-amd64.tar.gz | tar -xz && ./install.sh && rm install.sh
```

#### Linux (AMD64)
```bash
curl -L https://github.com/KKyosuke/usagi/releases/latest/download/usagi-linux-amd64.tar.gz | tar -xz && ./install.sh && rm install.sh
```

#### Windows (AMD64)
[Releases](https://github.com/KKyosuke/usagi/releases) ページから `usagi-windows-amd64.zip` をダウンロードして展開し、Git Bash で `install.sh` を実行するか、バイナリを手動で PATH に追加してください。

### From Source

```bash
cargo install --path .
```

## Quick Start

依存ツール・通知・設定ストレージの健全性確認:

```bash
cargo run -- doctor
```

`git` / `bash` の導入状況に加え、`usagi hop` のデスクトップ通知が利用可能か、設定ストレージが読めるかを `ok` / `warn` / `missing` で表示します。

### ワークスペースで開発を始める

```bash
cd <project>      # usagi init 済みのプロジェクト
cargo run -- hop  # TUI を起動
```

ホーム画面は「いまどの立場で操作しているか」を **4 つのモード**で切り替えます。`Ctrl-O` で一段ズームアウト、`Esc` で一段戻り、`Ctrl+C` で終了します。

| モード | 役割 | 主な操作 |
|---|---|---|
| **統括**（Overview） | 既定。ワークスペース全体を操作 | 下部コマンドラインで `session` / `config` を実行 |
| **切替**（Switch） | セッションの選択・新規作成 | 左ペインで `↑↓` 選択・`←→`（または `Ctrl-N`/`Ctrl-P`）でタブ切替・`Enter` 確定・`c` で新規作成・`r` で表示名変更 |
| **在席**（Focus） | 選択中セッションのコマンド | 右ペインで `terminal` / `agent` を起動・`Ctrl-N`/`Ctrl-P` でタブ切替 |
| **没入**（Attached） | 埋め込みシェル / Agent | ライブ端末を直接操作（予約キーは `Ctrl-O` と `Ctrl-N`/`Ctrl-P`。`Ctrl-N`/`Ctrl-P` で没入のままタブを前後に切替）。マウス左ドラッグでテキストを選択し、離すとコピー。リンクを左クリックすると既定のブラウザで開く |

典型的な流れ:

```text
session create feature-x   # .usagi/sessions/feature-x/ にセッション（worktree）を作成（短縮形 c / new）
session switch             # 切替モードに入りセッションを選ぶ（一覧から ↑↓・Enter。c で新規作成・r で表示名変更）
agent                      # 選んだセッションで Agent CLI（既定 claude）を埋め込み起動 → 没入
```

`agent` は選択中セッションの worktree でシェルを右ペインに埋め込み、設定中の Agent CLI（既定 `claude`、Config・ローカル設定で変更可）を起動します。このとき usagi の MCP サーバ（後述）を組み込むため、エージェントは起動直後から `issue_*` tool でタスクを、`memory_*` tool でメモリを操作できます（Claude は `--mcp-config` で注入。Gemini は `settings.json` 経由のため現状は組み込みません）。素のシェルだけ欲しいときは `terminal` を使います。

各セッションのシェルは画面を開いている間プールに常駐するので、`Ctrl-O` で切替へズームアウトして別セッションへ移っても、裏で `claude` は動き続けます。左ペインは各セッションを 2 行で表示し、**稼働中は `▶ running`（緑）／入力待ちは `◆ waiting`（黄色）／アイドルは `⏸ idle`（シアン）** でひと目で状態がわかります。アタッチしていないセッションが入力待ちになるとデスクトップ通知（`🐰 <ブランチ名> が入力待ちです`）も出るため、複数セッションを並行で走らせ、入力が必要になったものだけに対応できます（通知は `notifications_enabled` が ON のとき。状態は `claude` のライフサイクルフックで判定し、フックを持たない Agent ではターミナルベルで推定します。詳細は [document/04-orchestration.md#Agent フックによる状態報告](document/04-orchestration.md#agent-フックによる状態報告)）。

ホーム画面を開くと、実行中ビルドより新しいリリースが公開されていれば右上にうさぎのアスキーアートと「最新版があります v\<X.Y.Z\>」を表示します（GitHub のリリースタグをバックグラウンドで確認。差分が無い・オフライン時は何も出ません）。

画面・モード・キー操作の詳細は [document/design/05-home.md](document/design/05-home.md)、コマンドの仕様は [document/03-commands/02-tui.md](document/03-commands/02-tui.md) を参照してください。

### タスクを管理する

プロジェクトのタスクは `usagi issue` で管理できます（`<repo>/.usagi/issues/` に frontmatter 付き markdown で保存。git で共有されます）:

```bash
cargo run -- issue create --title "ログイン画面" --priority high --depends-on 1
cargo run -- issue list            # 着手可能(ready)/ブロック中を可視化
cargo run -- issue list --ready    # いま着手できる issue だけ
cargo run -- issue update 2 --status done
```

`list` / `search` は依存（`--depends-on`）がすべて `done` になった「着手可能」な issue を `ready` と表示し、ブロック中のものには未達の依存番号を併記します。詳細は [document/03-commands/01-cli.md](document/03-commands/01-cli.md#usagi-issue)。

### メモリを蓄積する

セッションをまたいで覚えておきたい知識は `usagi memory` で管理できます（`<repo>/.usagi/memory/` に frontmatter 付き markdown で保存。issue と同じく git で共有されます）。issue がタスクを管理するのに対し、メモリはユーザーの好み・作業指針・プロジェクト固有の前提・外部リソースへのポインタといった、コードや git からは読み取れない事実を蓄積します:

```bash
cargo run -- memory save --name tabs --title "ユーザーはタブを好む" --type user
cargo run -- memory list                 # updated_at の新しい順
cargo run -- memory search "デプロイ"     # 名前・タイトル・本文を全文検索
```

同じ名前への保存は上書き（in-place 更新）になり重複しません。保存・更新すると目次 `MEMORY.md` が再生成されます。詳細は [document/03-commands/01-cli.md](document/03-commands/01-cli.md#usagi-memory)。

### AI エージェントから使う（MCP）

`usagi mcp` で同じ issue・メモリ操作を MCP（Model Context Protocol）サーバとして公開できます。Claude Code などに登録すると、1 つのサーバでエージェントが `issue_create` / `issue_list` / `issue_update` などの tool でタスクを、`memory_save` / `memory_list` / `memory_search` などの tool でメモリを操作できます。

```json
{
  "mcpServers": {
    "usagi": { "command": "usagi", "args": ["mcp"] }
  }
}
```

詳細は [document/03-commands/03-mcp.md](document/03-commands/03-mcp.md)。

### ローカル LLM でトークンを節約する（任意）

ローカルで動く LLM（[Ollama](https://ollama.com)）を MCP サーバとして公開し、要約・命名・定型文生成などの**軽量タスクをローカル LLM に委譲**することで、クラウド Agent（Claude など）のトークン消費を抑えられます。**既定は無効**で、usagi が勝手に有効化することはありません。

- **有効化**: Config 画面（`config`）または `settings.json` の `local_llm.enabled` を `true` にします。委譲先モデルは `local_llm.model`（既定 `qwen2.5-coder:7b`）。
- **資材の導入**: `ollama` 本体やモデルが無い場合、Config 画面では `Local LLM` 行が `Install` と表示されます。`Space` / `Enter` でインストールモーダルを開き、sudo パスワードを入力して確定すると、公式インストーラ（`curl … | sh`）をバックグラウンドで実行します（スピナー表示）。完了すると on/off トグルに変わり、続けてモデルを選べます。`usagi doctor --fix` でも導入できます。
- **Agent への組み込み**: 有効時、`agent` 起動コマンドに `usagi-llm` サーバ（`usagi llm-mcp`）が自動で追加され、エージェントは `local_llm_ask` tool でローカル LLM に問い合わせられます。

詳細は [document/03-commands/04-llm-mcp.md](document/03-commands/04-llm-mcp.md)。

## Project Structure

クリーンアーキテクチャを採用しています（domain → usecase → infrastructure ← presentation）。

```
src/
├── main.rs            # CLI エントリポイント (clap)
├── lib.rs             # モジュール宣言
├── domain/            # ビジネスエンティティ（外部依存なし）
├── usecase/           # ビジネスロジック
├── infrastructure/    # 永続化・Git 操作などの外部連携
└── presentation/      # CLI/TUI インターフェース
tests/                 # 統合テスト
document/              # プロジェクトドキュメント（仕様・規約。開発者 + AI 向け）
.agents/               # AI エージェント固有の作業手順（CLAUDE.md/GEMINI.md から参照）
```

> `document/` は開発者・AI の双方が読むプロジェクト仕様と[開発規約](document/06-conventions.md)、`.agents/` は AI に守らせる作業手順（worktree 運用・PR の進め方など）を置きます。仕様の目次は [document/README.md](document/README.md) を参照してください。

## Development

```bash
cargo build          # ビルド
cargo test           # テスト
cargo fmt            # フォーマット
cargo clippy         # Lint
```

### Git Hooks

[lefthook](https://lefthook.dev) で Git hooks を管理しています。クローン後に一度だけ実行してください:

```bash
brew install lefthook   # macOS 以外: npm i -g lefthook など
lefthook install
```

| フック | 内容 |
| --- | --- |
| pre-commit | ブランチ名チェック / staged な `.rs` を `cargo fmt` で自動フォーマット |
| commit-msg | [Conventional Commits](https://www.conventionalcommits.org/ja/) 形式のチェック |
| pre-push | `cargo clippy -- -D warnings` / `cargo test`（CI と同条件） |

- ブランチ名: `<type>/<説明>`（例: `feat/add-doctor-command`）
- コミットメッセージ: `<type>[(scope)][!]: <説明>`（例: `feat: doctor コマンドを追加`）
- type: `feat` `fix` `docs` `style` `refactor` `perf` `test` `build` `ci` `chore` `revert`
- 緊急時のスキップ: `LEFTHOOK=0 git commit ...` または `git commit --no-verify`

### Release

リリースは `Cargo.toml` の `version` 変更を起点に自動化されています。`version` を上げる変更を `main` にマージすると、`v<version>` タグと GitHub Release が自動作成され、各プラットフォーム向けバイナリが添付されます。リリースノートは GitHub Models（AI）がコミットログから自動生成します。詳細は [document/06-conventions.md#リリース](document/06-conventions.md#リリース) を参照してください。

## License

MIT
