<div align="center">

<pre>
   (\(\                        
   (='-')     ╻ ╻ ┏━┓ ┏━┓ ┏━╸ ╻
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

`usagi hop` を実行すると、うさぎのマスコットと `USAGI` タイトルがフェードインするスプラッシュから始まり、ワークスペースを開くとホーム画面へ遷移します。プロジェクト選択画面では、登録済みワークスペースをアルファベット順で表示し、通常文字を入力すると検索バー（フィルター）へそのまま入り、名前で絞り込めます。`Single │ Unite` タブは `←` / `→` で切り替えられ、通常は `Enter` でカーソル行を単品で開きます。`Unite` では必要なワークスペースをチェックして同時に開けます（複数のプロジェクトのセッションを 1 画面に積み重ねて操作できます）。

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

**ホーム画面（切替モード）**

```text
        usagi · ▸ root · 4 sessions
        Switch › Focus › Attached
 ▎ ⌂   root              │
 ▎     workspace root    │
       ──────────────    │
 > ●   main       pushed │   (右ペインは
     ▶ running           │    モードで変化)
   ○   feat/login  local │
     ◆ waiting           │
```

</td>
</tr>
</table>

左ペインは各セッションを 2 行で表示し、稼働中は **`▶ running`（緑）**／入力待ちは **`◆ waiting`（黄）**／アイドルは **`⏸ idle`（シアン）** でひと目で状態がわかります。`Ctrl-O` で切替へズームアウト、`Esc` で一段戻り、`:` でコマンドパレット、`Ctrl+C` / `Ctrl+Q` で終了します（`Ctrl+Q` は没入中でも効き、終了前に必ず確認モーダルを出します）。画面・モード・キー操作の詳細は [document/design/home/README.md](document/design/home/README.md) を参照してください。

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

新しいリリースがあるときは、ホーム画面のマスコット（うさぎ）が吹き出しで知らせます。**うさぎをクリック → 確認モーダルで `y`** を選ぶと、このインストールスクリプトをバックグラウンドで再実行してアップデートできます（反映するには usagi の再起動が必要）。詳細は [アップデート確認モーダル](document/design/home/05-overlays.md#アップデート確認モーダル) を参照してください。

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

### Tab 補完

`usagi completion <shell>` は、シェルに読み込ませる補完スクリプトを標準出力へ印字します。読み込み後は
`usagi <TAB>` でサブコマンドやフラグを補完できます。

```bash
source <(usagi completion bash)                         # bash（現在のシェル）
usagi completion zsh > ~/.zfunc/_usagi                  # zsh（fpath/compinit で読み込み）
usagi completion fish > ~/.config/fish/completions/usagi.fish
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

ホーム画面は「いまどの立場で操作しているか」を **3 つのモード**で切り替えます（起動直後の既定は**切替**）。`Ctrl-O` で切替へズームアウト、`Esc` で一段戻り、`Ctrl+C` / `Ctrl+Q` で終了します。ワークスペース全体のコマンドは `:`（コロン）で開く**コマンドパレット**から実行します。

| モード | 役割 | 主な操作 |
|---|---|---|
| **切替**（Switch） | 既定。セッションの選択・新規作成 | 左ペインで `↑↓` 選択・`←→`（または `Ctrl-N`/`Ctrl-P`）でタブ切替・`Enter` 確定・`c` で新規作成・`r` で表示名変更・`n` でメモ編集（選択中の行のメモ＝次回 TODO は右ペインに表示。ルート行はワークスペースルートのメモ） |
| **在席**（Focus） | 選択中セッションのコマンド | 右ペインで `terminal` / `agent` を起動・`Ctrl-N`/`Ctrl-P` でタブ切替・`Ctrl-E` でメモ編集 |
| **没入**（Attached） | 埋め込みシェル / Agent | ライブ端末を直接操作（予約キーは `Ctrl-O`・`Ctrl-N`/`Ctrl-P`・`Ctrl-E`（メモ編集）。`Ctrl-N`/`Ctrl-P` で没入のままタブを前後に切替）。マウス左ドラッグでテキストを選択し、離すとコピー。リンクを左クリックすると既定のブラウザで開く |
| コマンドパレット（**統括** / Overview） | ワークスペース全体のコマンド（常駐モードではない） | `:` で開き、`session` / `unite` / `config` / `issue` などを実行。`Esc` で閉じて元のモードへ戻る |

典型的な流れ:

```text
:                          # コマンドパレットを開く
session create feature-x   # .usagi/sessions/feature-x/ にセッション（worktree）を作成（短縮形 c / new。待機中に他操作がなければ → 在席）
agent                      # 選んだセッションで Agent CLI（既定 claude）を埋め込み起動 → 没入
```

`agent` は選択中セッションの worktree でシェルを右ペインに埋め込み、設定中の Agent CLI（既定 `claude`、Config・ローカル設定で変更可）を起動します。このとき usagi の MCP サーバ（後述）を組み込むため、エージェントは起動直後から `issue_*` tool でタスクを、`memory_*` tool でメモリを操作できます（Claude は `--mcp-config`、Codex は `-c` 設定上書きで注入。Codex 互換の `codex-fugu` も同方式で組み込みます。Gemini はインライン注入フラグが無いため MCP は組み込まず、`-r latest` での会話再開と `-i` での初期プロンプトのみ配線します）。素のシェルだけ欲しいときは `terminal` を使います。`terminal` / `terminal open` は usagi 内の埋め込みタブを追加し、`terminal new` は同じディレクトリで OS ネイティブの新規ターミナルを開きます。

各セッションのシェルは画面を開いている間プールに常駐するので、`Ctrl-O` で切替へズームアウトして別セッションへ移っても、裏で `claude` は動き続けます。usagi を終了して再び開いたときも、前回開いていたペイン（agent / terminal）を起動時にバックグラウンドで復旧し、agent は前回の会話の続きから再開します（設定 `restore_panes_enabled`、既定 ON。[document/04-orchestration.md#ペインの復旧](document/04-orchestration.md#ペインの復旧)）。左ペインは各セッションを 2 行で表示し、**稼働中は `▶ running`（緑）／入力待ちは `◆ waiting`（黄色）／アイドルは `⏸ idle`（シアン）** でひと目で状態がわかります。アタッチしていないセッションが入力待ちになるとデスクトップ通知（`🐰 <ブランチ名> が入力待ちです`）も出るため、複数セッションを並行で走らせ、入力が必要になったものだけに対応できます（通知は `notifications_enabled` が ON のとき。状態は `claude` / `codex` のライフサイクルフックで判定し、フックを持たない Agent ではターミナルベルで推定します。詳細は [document/04-orchestration.md#Agent フックによる状態報告](document/04-orchestration.md#agent-フックによる状態報告)）。

ホーム画面を開くと、実行中ビルドより新しいリリースが公開されていれば右上にうさぎのアスキーアートと「アップデートがあるぴょん v\<X.Y.Z\>」を表示します（GitHub のリリースタグをバックグラウンドで確認。差分が無い・オフライン時は何も出ません）。

画面・モード・キー操作の詳細は [document/design/home/README.md](document/design/home/README.md)、コマンドの仕様は [document/03-commands/02-tui.md](document/03-commands/02-tui.md) を参照してください。

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

### 1Password から secret を読む（MCP）

`usagi op-mcp` で [1Password CLI（`op`）](https://developer.1password.com/docs/cli/)を MCP サーバとして公開できます。エージェントは、タスクに必要な資格情報（API キー・トークンなど）を**その場で 1Password から読み取れる**ため、秘密情報をプロンプトに貼り付けずに済みます。公開する tool は**読み取り専用**（`op_read` / `op_item_get` / `op_item_list` / `op_vault_list` / `op_whoami`）です。

- **有効化**: `usagi op login` で **1Password サービスアカウントトークン**を OS のシークレットストア（macOS Keychain / Linux Secret Service）に保存します。同時に `op_mcp.enabled = true` になり、`agent` 起動時に `usagi-op` サーバ（`usagi op-mcp`）が自動で wire されます。削除は `usagi op logout`、状態確認は `usagi op status`。
- **トークンの扱い**: トークンは `settings.json` には保存せず、OS シークレットストアから `usagi op-mcp` プロセスが読み取り、`op` へ環境変数 `OP_SERVICE_ACCOUNT_TOKEN` として渡します（エージェントの起動コマンド行やプロセス一覧には出ません）。

詳細は [document/03-commands/05-op-mcp.md](document/03-commands/05-op-mcp.md)。

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
.agents/               # AI エージェント固有の作業手順（CLAUDE.md/AGENTS.md/GEMINI.md から参照）
```

> `document/` は開発者・AI の双方が読むプロジェクト仕様と[開発規約](document/06-conventions.md)、`.agents/` は AI に守らせる作業手順（worktree 運用・PR の進め方など）を置きます。仕様の目次は [document/README.md](document/README.md) を参照してください。

## Development

```bash
cargo build          # ビルド
cargo test           # テスト
```

コミット・push 前の品質チェック（`cargo fmt` / `cargo clippy --all-targets -- -D warnings` / `cargo test`）、ブランチ名・コミットメッセージ規約、CI の詳細は開発規約の正本 [document/06-conventions.md](document/06-conventions.md) を参照してください。

### Git Hooks

[lefthook](https://lefthook.dev) で Git hooks を管理しています。クローン後に一度だけ実行してください:

```bash
brew install lefthook   # macOS 以外: npm i -g lefthook など
lefthook install
```

各フック（pre-commit / commit-msg / pre-push）の内容と緊急時のスキップ方法は [document/06-conventions.md#git-hookslefthook](document/06-conventions.md#git-hookslefthook) を参照してください。

### Release

リリースは `Cargo.toml` の `version` 変更を起点に自動化されています。`version` を上げる変更を `main` にマージすると、`v<version>` タグと GitHub Release が自動作成され、各プラットフォーム向けバイナリが添付されます。リリースノートは GitHub Models（AI）がコミットログから自動生成します。詳細は [document/06-conventions.md#リリース](document/06-conventions.md#リリース) を参照してください。

## License

MIT
