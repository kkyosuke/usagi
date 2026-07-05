# 3.1 CLI コマンド

> [コマンドリファレンス目次](README.md) ｜ 次へ → [TUI 内コマンド](02-tui.md)

シェルから `usagi <cmd>` で実行する CLI コマンドの一覧です。

`issue` / `memory` / `mcp` / `llm-mcp` は **AI エージェントが MCP 経由で扱うためのコマンド**で、`usagi --help` の一覧には表示しません（実行自体は可能）。人手で叩くものではないため、ヘルプを汚さないよう隠しています。`agent-phase`（エージェントのライフサイクルフックが状態を報告するために呼ぶ内部コマンド）と `guard-workspace`（Claude の `PreToolUse` フックがツールの対象パスを検査し、worktree の外への読み書きを拒否する内部コマンド）も同様に隠しコマンドで、人手で実行するものではありません。

`config` も `usagi --help` には表示しません。設定変更の導線は起動画面の Config（`usagi hop` → Config）に揃え、CLI の `config` は raw な設定確認・`--edit` で TUI が表示しない項目（例: `workspace_root`）を編集する上級者向けの互換コマンドとして残します。

## 目次

- [CLI コマンド一覧](#cli-コマンド一覧)
  - [`usagi issue`](#usagi-issue)
  - [`usagi memory`](#usagi-memory)
  - [`usagi mcp`](#usagi-mcp)
  - [`usagi llm-mcp`](#usagi-llm-mcp)

## CLI コマンド一覧

| コマンド | 説明 |
|---|---|
| `usagi init` | カレントディレクトリをプロジェクトとして登録する（`.usagi/` を初期化し、グローバルレジストリ `workspaces.json` に追加） |
| `usagi init --git <URL>` | カレントディレクトリ配下に `<リポジトリ名>/` を作成して clone し、プロジェクトとして登録する |
| `usagi init-agent [--yes]` | AI エージェント用の設定ファイル（`CLAUDE.md`, `.clinerules`, `.aider.conf.yml`）をプロジェクトの言語/構成に応じて自動生成する（`--yes` または `-y` で上書き確認をスキップ） |
| `usagi` / `usagi hop` | メインの TUI を起動する。起動画面 → プロジェクト選択 → ホーム画面へ遷移（[design/](../design/README.md)）。サブコマンドを省略した `usagi` は `usagi hop` と同じ |
| `usagi run [N]` | うさぎアニメを全画面で再生して見るギャラリー。`N`（1–5）で種類を選ぶ（既定 1）。なにかキーで終了 |
| `usagi icon [view]` | 正方形ピクセルで組んだうさぎマークをブロック文字（`█▀▄▘▝▖▗…`）で標準出力に印字する。`view`（`all` / `flip` / `half`、既定 `all`）で表示を選ぶ（下記） |
| `usagi status` | カレントリポジトリの worktree 状態を `.usagi/state.json` に同期し一覧表示する（[data/02-workspace.md](../data/02-workspace.md)） |
| `usagi update [--dry-run]` | 各リポジトリのデフォルトブランチ（`main` など）を `origin` から取得して最新化し、その更新を各セッション worktree に配布（コンフリクトしないところだけマージ）する。`--dry-run` は変更せずに結果のみ表示 |
| `usagi clean [--dry-run] [--agent <NAME>]` | 設定中の Agent CLI をヘッドレスでバックグラウンド起動し、放置・マージ済みのセッション worktree を AI が自律的に判断して削除する。即座に制御を返し、出力は `.usagi/clean.log` に追記する |
| `usagi completion <shell>` | 指定したシェル（`bash` / `zsh` / `fish` / `powershell` / `elvish`）の補完スクリプトを標準出力に印字する。シェルに読み込ませると `usagi <TAB>` でサブコマンド・フラグ・値が Tab 補完できる |
| `usagi feature` | 各 Agent CLI（Claude / Codex / sakana.ai / Gemini / Antigravity）が usagi のどの機能に対応しているかを、端末向けの表で表示する（下記） |
| `usagi op login` | 1Password のサービスアカウントトークンを OS のキーチェーンに保存する。workspace の `op://` 環境変数（`env`）を非対話環境でも解決できるようにする（下記） |
| `usagi config` | （ヘルプ非表示・上級者向け）現在のグローバル設定（`settings.json`）を一覧表示する（[5. 設定](../05-settings.md)） |
| `usagi config --edit` | （ヘルプ非表示・上級者向け）グローバル設定ファイルを `$EDITOR` で開いて編集し、保存時に形式（JSON / 必須 `version` / 型）を検証する。不正な場合は直前の内容に巻き戻す |
| `usagi doctor` | `git` / `bash` と各 Agent CLI（Claude / Codex / sakana.ai / Gemini / Antigravity）の導入状況、デスクトップ通知の可否、Nerd Font の有無、設定ストレージの健全性を確認する（ローカル LLM 有効時は `ollama`・モデルも）。インストール可能な不足（必須ツール・Nerd Font）があれば `[y/N]` で確認し、`y` で導入する |
| `usagi doctor --fix` | 確認を省いて不足を一括導入する。不足ツールを OS のパッケージマネージャ（brew / apt-get / dnf / pacman）で導入を試行し、Linux のシステムパッケージは `sudo -n` で非対話に失敗させてパスワード待ちで固まらないようにする。修復不可なら手動手順を提示する。Nerd Font が無ければ `curl`/`unzip` でダウンロードして導入する。ローカル LLM が有効なら `ollama`・サーバ起動・モデルも導入する |
| `usagi issue <create\|list\|graph\|show\|update\|search\|delete>` | （ヘルプ非表示・エージェント向け）カレントリポジトリのタスク issue（`.usagi/issues/`）を操作する（[data/02-workspace.md](../data/02-workspace.md#issues-タスク-issue)） |
| `usagi memory <save\|list\|show\|update\|search\|delete>` | （ヘルプ非表示・エージェント向け）カレントリポジトリのエージェントのメモリ（`.usagi/memory/`）を操作する（[data/04-memory.md](../data/04-memory.md)） |
| `usagi mcp` | （ヘルプ非表示・エージェント向け）issue・メモリ・セッションの操作を MCP（Model Context Protocol）サーバとして stdio で公開し、AI エージェントから使えるようにする |
| `usagi llm-mcp [--model <MODEL>]` | （ヘルプ非表示・エージェント向け）ローカル LLM（Ollama）を MCP サーバとして公開し、クラウド Agent が軽量タスクを委譲できるようにする（トークン節約） |

### `usagi init`

カレントディレクトリ（または `--git` 指定時はクローン先）を usagi のワークスペースとして登録します。

- `.usagi/` を初期化し、グローバルレジストリ `~/.usagi/workspaces.json` にエントリを追加。
- `.usagi/.gitignore` を生成してローカル状態を無視する設定を自己完結で書き込む（ただし共有対象の `.usagi/issues/` は追跡。リポジトリルートの `.gitignore` は汚さない。詳細は [data/02-workspace.md](../data/02-workspace.md#保存場所)）。
- `--git <URL>` 指定時は、カレントディレクトリ配下に `<リポジトリ名>/` を作って `git clone` してから登録。`<URL>` は `https` / `http` / `ssh` / `git` スキーム and scp 形式（`git@host:owner/repo.git`）のみ許可し、コマンド実行につながる git リモートヘルパー（`ext::` など）や `file://` は拒否する。

### `usagi init-agent`

AI エージェント用の設定ファイル（`CLAUDE.md` / `.clinerules` / `.aider.conf.yml`）をプロジェクトディレクトリに自動生成します。

- プロジェクト内のファイル（`Cargo.toml`, `package.json`, `requirements.txt` など）を走査して主要な開発言語を自動検出し、その言語に応じた推奨のビルド・テスト・Lint・フォーマット用のコマンドおよびガイドラインを初期設定します。
- すでに設定ファイルが存在する場合は、ファイルごとに上書き確認を行います。
- `--yes`（または `-y`）オプションを指定すると、既存ファイルを上書き確認なしで強制的に上書きします。

### `usagi hop`

TUI を起動します。代替スクリーン上で起動画面を表示し、Open / New / Config / Quit を選べます。
画面遷移とキー操作は [design/README.md](../design/README.md) を参照してください。

サブコマンドを省略して `usagi` だけを実行した場合も `usagi hop` と同じく TUI を起動します。

### `usagi run`

うさぎのアニメーションを全画面で再生して確認するためのギャラリーです。引数 `N`（1–5、既定 `1`）で
再生する種類を選び、なにかキーを押すまでループ再生します（`Ctrl+C` でも終了）。代替スクリーン上で
描画し、終了時に端末を復帰します。

| `N` | アニメーション |
|---|---|
| 1 | 走り回るうさぎ（左右に往復しながら跳ねる） |
| 2 | 増えていくうさぎ（左端から右へ 1 匹ずつ増えていく） |
| 3 | 読み込み（ホップ＋ブレイルスピナー） |
| 4 | 読み込み（表情が時間で変わる） |
| 5 | マスコット（静止。起動画面と同じ AA） |

> アニメーションの絵柄そのものは共通ウィジェット（`src/presentation/tui/widgets/`）にあり、`usagi run` は
> それを全画面で再生する薄い画面（`src/presentation/tui/gallery/`）です。

### `usagi icon`

正方形のマス目（`'#'` 塗り / `'.'` 空き）で設計したうさぎのロゴマークを、ブロック文字で標準出力に印字します。`usagi run` のアニメ（顔文字 AA）とは別物で、こちらは**四角の塊と反転だけで組む静的なマーク**です。引数 `view`（既定 `all`）で表示を選びます。

| `view` | 表示 |
|---|---|
| `flip` | 横向きの原型（右向き）とその**水平反転**（左向き）を左右に並べる。四分割ブロック（`▘▝▖▗`）で 2×2 マスを 1 文字に畳んで小型化し、片側を設計して反転すれば逆向きが得られることを示す |
| `half` | 正面向きの頭部を**半マス（`▀▄`）**で描く。上下 2 マスを 1 文字に畳み、縦を半分に圧縮する |
| `all`（既定） | 上記すべてを順に表示 |

- 描画はブロック文字のみで、2 種類の圧縮を使い分けます。
  - **半マス**: 上下 2 マスを `█` / `▀` / `▄` / 空白に畳み、**縦を半分**にする表現。
  - **四分割**: 2×2 マスを `▘▝▖▗▀▄▌▐▞▚▛▜▙▟█` の 16 種から選び、**縦横とも半分**にする最小表現。
- マークの絵柄は `src/presentation/cli/icon.rs` にマス目データ（`PROFILE` / `MINI`）として持ち、純粋な描画関数が `Vec<String>` を組み立てます。

### `usagi status`

`git worktree list` などを読み取り専用で検査し、`<repo>/.usagi/state.json` を同期したうえで、
各 worktree のブランチ・HEAD・`local` / `pushed` / `synced`（up to date）状態を一覧表示します。

### `usagi update`

ワークスペースのデフォルトブランチを最新化し、その更新を各セッションの worktree に配布します。`git` への副作用はあるものの、**コンフリクトするセッションには一切触れません**（マージは衝突した時点で自動的に abort して worktree を元に戻します）。処理は 2 段階です。

1. **ルートの最新化**: ワークスペース内の各リポジトリ（[新規作業](../../.agents/workflow.md) と同じ走査で見つける source repo）について `git fetch origin` し、デフォルトブランチ（`origin/HEAD` から検出。例 `main`）を **fast-forward だけ**で `origin` に追従させます。デフォルトブランチがチェックアウトされていない・未コミットの変更がある・ローカルが先行して fast-forward できない場合は、そのリポジトリには触れずスキップ理由を表示します（マージコミットは作りません）。
2. **セッションへの配布**: `.usagi/state.json` に記録された各セッションの worktree（`usagi/<name>` ブランチ）に、最新化したデフォルトブランチを `git merge` で取り込みます。**クリーンにマージできるところだけ**取り込み、未コミットの変更がある worktree はスキップ、コンフリクトするマージは abort してスキップします。

セッション worktree の中（`.usagi/sessions/<name>/`）から実行しても、ワークスペース全体を対象に動作します。

- `--dry-run`: `origin` の取得（fetch）は行いますが、ローカルのブランチや worktree は一切変更せず、「何が更新されるか / スキップされるか」だけを表示します。

各リポジトリ・各 worktree の結果は `already up to date` / `fast-forwarded (N commits new)` / `merged (N commits new)` / `skipped (...)` のように一覧表示されます。

### `usagi clean`

放置された不要なセッション worktree の整理を、設定中の Agent CLI（Claude / Codex / sakana.ai / Gemini / Antigravity）に任せます。
usagi は対象を自分で判断せず、Agent CLI を**ヘッドレス（非対話）かつバックグラウンドにデタッチ起動**し、即座に制御を返します。

- 起動した AI には「`.usagi/sessions/<name>/` 配下のマージ済み・放置されたセッション worktree を調べ、不要なものを削除する」タスクを与えます。
  AI は usagi の MCP セッションツール（`session_list` / `session_remove`）や git を使って自律的に削除まで実行します（事前確認なし）。
  リポジトリ本体・`main/`・未コミットの変更が残る worktree には触れません。
- ヘッドレス起動時は無人実行のため、各 CLI の権限承認をバイパスするフラグを付けて起動します
  （Claude: `--dangerously-skip-permissions`、Codex: `--dangerously-bypass-approvals-and-sandbox`、Gemini: `--yolo`、Antigravity: `--dangerously-skip-permissions`）。
- AI の標準出力・標準エラーは `<workspace>/.usagi/clean.log` に追記します。起動した CLI 名とログの場所を表示して終了します。
- `--dry-run`: AI に削除させず、削除すべきと判断した対象とその理由だけを報告させます。
- `--agent <NAME>`: 設定の既定 CLI を上書きし、この実行で使う CLI を指定します（`claude` / `codex` / `codex-fugu` / `gemini` / `agy`、表示名 `sakana.ai` / `antigravity` も可）。

> Gemini・Antigravity は MCP のインライン注入経路を持たないため、起動しても usagi の MCP セッションツールを使えず git のみで作業します。

### `usagi completion`

指定したシェル向けの補完スクリプトを標準出力に印字します。出力をシェルに読み込ませると、`usagi <TAB>` で
サブコマンド・フラグ・値（`value_enum` 引数の候補など）が Tab 補完できるようになります。補完定義は usagi 本体が
解析に使う `clap` のコマンドツリーから生成されるため、CLI の実態と常に一致します。

- 引数 `<shell>`: `bash` / `zsh` / `fish` / `powershell` / `elvish` のいずれか。
- スクリプトは標準出力に印字するだけなので、各シェルの補完読み込みパスに保存するか、起動ファイルで読み込ませます。

```bash
# bash（現在のシェルで一時的に有効化）
source <(usagi completion bash)

# zsh（補完ディレクトリへ保存して恒久化する例）
usagi completion zsh > ~/.zfunc/_usagi   # その後 fpath に ~/.zfunc を追加し compinit を実行

# fish
usagi completion fish > ~/.config/fish/completions/usagi.fish
```

### `usagi feature`

`agent` で起動する各 Agent CLI（Claude / Codex / sakana.ai / Gemini / Antigravity）が、usagi のどの統合機能に対応しているかを
端末向けの罫線付き表で表示します。行は機能（MCP / ローカル LLM 委譲 / 状態報告（フック）/ 初期プロンプト /
system prompt 注入 / 会話の再開 / 会話履歴の破棄）、列は CLI で、`✓ yes`（usagi が配線）/ `— no`（CLI 制約により非対応）で示します。

- 対応状況の正本は `domain/agent_feature.rs`（CLI ごと・機能ごとの対応を一元管理）です。
- `sakana.ai`（起動コマンド `codex-fugu`、rollout は `~/.codex-fugu`）は Codex 互換 CLI で、Codex と同じ統合機能をすべて受けます。
- Gemini・Antigravity（起動コマンド `agy`、Gemini CLI の後継）は MCP・フック・system prompt のインライン注入経路を持たないため非対応で、状態はターミナルベルで推定します
  （詳細は [4. オーケストレーション#agent-フックによる状態報告](../04-orchestration.md#agent-フックによる状態報告)）。両者とも初期プロンプト・会話の再開・会話履歴の破棄は配線します。

### `usagi op`

1Password のサービスアカウントトークンを OS ネイティブのシークレットストア（macOS: Keychain、Windows: Credential Manager、Linux: カーネル keyutils）に保存します。保存したトークンは workspace の secret 環境変数（[`env`](../05-settings.md)）を解決するときに使われます。

- `usagi op login`: プロンプトに従ってトークンを貼り付けます（入力はエコーしません）。保存先はキーチェーンで、`settings.json` には平文で残しません。同じ手順で叩き直せばトークンを更新（ローテーション）できます。
- 保存したトークンは、`agent` / `terminal` の embedded pane 起動時に usagi が `op read --no-newline` で `op://` reference を解決する際、`OP_SERVICE_ACCOUNT_TOKEN` として `op` に渡されます（コマンド引数ではなく環境変数経由なので、プロセス一覧に載りません）。
- トークン未保存でも、`op signin` セッションなど `op` CLI 側の通常の認証があればそのまま解決できます（[5. 設定#env](../05-settings.md)）。

### `usagi config`

`config` は `usagi --help` の一覧には表示しない上級者向けコマンドです。通常の設定変更は起動画面の Config（`usagi hop` → Config）を使います。

usagi の設定ファイル（グローバルな `settings.json`、`~/.usagi/` または `$USAGI_HOME` 配下）を扱います。

- 引数なし: 現在の設定を `key  value` 形式で一覧表示します。
- `--edit`: 設定ファイルを `$EDITOR`（→ `$VISUAL` → OS 既定の `vi` / `notepad`）で開いて編集します。
  `$EDITOR="code --wait"` のように引数付きの値も POSIX シェル規則で分割して扱います（シェルは起動しません）。
  保存後に再パースして形式（JSON 構文・必須 `version`・各フィールドの型）を検証し、不正な場合は
  **編集前の内容へ巻き戻して** エラーを表示するため、設定ミスで usagi が壊れません。

### `usagi doctor`

依存ツールの導入状況を診断します。システムの `git` などを読み取り専用で確認し、ユーザーの
環境設定を尊重します。

診断結果は `usagi doctor` 見出しの下に 1 行 1 チェックで表示します。各行は `✓ ok` / `! warn` /
`✗ missing` の状態、チェック名、詳細（検出したコマンド名・保存先・不足理由など）を列にそろえて並べ、
最後に `summary: <ok> ok, <warn> warn, <missing> missing` で集計を表示します。

Agent CLI（Claude / Codex / sakana.ai / Gemini / Antigravity）の有無も確認します。いずれも任意（usagi が起動するのは設定中の 1 つだけ）なので、未導入は `missing` ではなく `warn` 扱いで、`doctor` は正常終了し `--fix` の対象にもしません。各行は表示名で示し、`ok` のときは探索したコマンド名（`sakana.ai` なら `codex-fugu`、`Antigravity` なら `agy`）を併記します。

`nerd font` は、TUI が git ライフサイクルや issue グラフのグリフ描画に使う Nerd Font の有無を、ユーザーのフォントディレクトリ（macOS: `~/Library/Fonts`、Linux: `~/.local/share/fonts` / `~/.fonts`）を走査して確認します。Nerd Font は任意（未導入でも色付きの語にフォールバックする）なので、未導入は `warn` 扱いです。

診断を表示したあと、**インストール可能な不足**があれば導入に進みます。インストール可能な不足とは、`missing` の必須ツール（`git` / `bash`）と未導入の Nerd Font です。任意の Agent CLI（`warn`）と設定ストレージ（`config`）は対象外です（前者は導入が任意、後者は初回起動時に生成されるものでパッケージ導入の対象ではない）。

`--fix` を付けない素の `usagi doctor` は、インストール可能な不足があるときだけ対象を提示して `[y/N]` で確認し、`y`（または `yes`、大文字小文字不問）の場合に導入します。それ以外の入力や、パイプ/CI など対話端末でない（入力が即 EOF になる）場合は導入せず、診断表示だけで終了します。`--fix` を付けると確認を省いて一括導入します。

導入処理では、`missing` の依存ツールを OS に合わせたパッケージマネージャでの導入を試行します
（macOS: `brew install`、Linux: 利用可能な `sudo apt-get` / `dnf` / `pacman` を優先順に選択）。
自動修復できない場合（パッケージマネージャ未検出・インストール失敗）は、手動インストール手順を提示します。

Nerd Font が未導入なら、Nerd Fonts の GitHub リリースから JetBrainsMono Nerd Font を `curl` で取得し、`unzip` でユーザーのフォントディレクトリへ展開します（Linux では `fc-cache` でフォントキャッシュも更新）。すでに導入済みなら再ダウンロードしません。パッケージマネージャ経由のインストールではないため、`ollama` と同じく汎用の不足ツール修復とは別フローで処理します。`curl`/`unzip` が無い、または対応するフォントディレクトリが無いプラットフォーム（Windows など）では、手動導入の案内を表示します。

ローカル LLM が有効な場合は、`ollama` 本体の導入に加えて Ollama サーバの起動を確認し、停止していれば
`ollama serve` をバックグラウンドで起動してからモデルを取得します（Homebrew 版 `ollama` はサーバを
常駐させないため、これがないとモデル取得や `local_llm_ask` が `could not connect to ollama server` で失敗します）。

### `usagi issue`

カレントリポジトリのタスク issue（`<repo>/.usagi/issues/`、[data/02-workspace.md](../data/02-workspace.md#issues-タスク-issue)）を操作します。

| サブコマンド | 説明 |
|---|---|
| `create --title <T> [--priority <p>] [--label <L>…] [--depends-on <N>…] [--related <N>…] [--parent <N>] [--milestone <名>] [--body <md>]` | issue を作成し、採番した番号を表示 |
| `list [--status <s>] [--priority <p>] [--label <L>] [--parent <N>] [--milestone <名>] [--group-by <軸>] [--ready]` | 一覧表示。`--group-by` で軸ごとにグループ化（進捗付き）、`--ready` で着手可能な issue だけに絞り込む |
| `graph` | 依存ツリー（issue を依存先の下にネスト）を進捗サマリ付きで表示 |
| `show <番号>` | 1 件の frontmatter + 本文を表示 |
| `update <番号> [--title …] [--status …] [--priority …] [--label <L>…] [--depends-on <N>…] [--related <N>…] [--parent <N>\|--clear-parent] [--milestone <名>\|--clear-milestone] [--body …]` | 指定したフィールドだけを更新 |
| `search <クエリ> [--status …] [--priority …] [--label …] [--parent <N>] [--milestone <名>] [--ready]` | タイトル・本文を大文字小文字を無視して全文検索（ASCII 以外も含む Unicode 単位で照合） |
| `delete <番号> --yes` | issue を削除（`--yes` 必須） |

- `create` / `list` / `show` / `update` / `search` は `--json` を付けると機械可読な JSON を出力します（スクリプトや MCP 連携向け。`delete` / `graph` は対象外。`list --json` はグループ化せず配列を返す）。
- **関連の表現**: `--depends-on` はブロックする先行条件、`--related` はブロックしない緩い関連、`--parent` は所属（Epic ⊃ サブタスク）、`--milestone` は束ね。`update` の `--clear-parent` / `--clear-milestone` で解除します。
- **着手可能（ready）の可視化**: `list` / `search` は各 issue が ready かを示します。ready = `dependson` に挙げた issue が**すべて `done`** で、かつ自身が未 `done`。ブロック中の issue には未達の依存番号（`(blocked by 1, 3)`）を併記するので、いま着手できるタスクが一目で分かります。
- **グループ化・グラフ・進捗**: `--group-by` は `status` / `priority` / `milestone` / `parent` を受け付け、グループごとに見出しと進捗サマリ（件数・完了率・ready 数・バー）を出します。`graph` は `dependson` の依存ツリーを描き、ダイヤモンドや循環は一度だけ展開して `↑` を付けます。
- **グラフの状態グリフ**: `graph` は各ノードの先頭に進捗が一目で分かるグリフを付けます。`✓` 完了（`done`）、`○` 着手可能（ready）、`⊘` 依存未達でブロック中（blocked）。TUI の `issue graph` はこれに加えて色分けします（[02-tui.md](02-tui.md#issue)）。

```
$ usagi issue list
#1   done         high   done      認証基盤を実装
#2   todo         medium ready     ログイン画面
#3   todo         low    blocked   ログアウト  (blocked by 2)

$ usagi issue graph
✓ #1 認証基盤を実装 [done]
└─ ○ #2 ログイン画面 [todo]
   └─ ⊘ #3 ログアウト [todo]

3 issues · 1 done (33%) · 1 ready  [######--------------]
```

### `usagi memory`

カレントリポジトリの AI エージェントのメモリ（`<repo>/.usagi/memory/`、[data/04-memory.md](../data/04-memory.md)）を操作します。issue がタスクを管理するのに対し、メモリはユーザーの好み・作業指針・プロジェクト固有の前提・外部リソースへのポインタといった、コードや git からは読み取れない事実を蓄積します。

| サブコマンド | 説明 |
|---|---|
| `save --name <名> --title <T> [--type <t>] [--related <名>…] [--body <md>]` | メモリを保存。**同名なら上書き**（in-place 更新）するので重複しない |
| `list [--type <t>]` | 一覧表示（`updated_at` の新しい順、`--type` でフィルタ） |
| `show <名>` | 1 件の frontmatter + 本文を表示 |
| `update <名> [--title …] [--type …] [--related <名>…] [--body …]` | 指定したフィールドだけを更新 |
| `search <クエリ> [--type <t>]` | 名前・タイトル・本文を大文字小文字を無視して全文検索（ASCII 以外も含む Unicode 単位で照合） |
| `delete <名> --yes` | メモリを削除（`--yes` 必須） |

- `--type` は `user` / `feedback` / `project` / `reference`（既定 `project`）。
- `--name` / `<名>` は与えた文字列をスラッグ化して識別子にします（例: `"User Prefers Tabs"` → `user-prefers-tabs`）。
- `save` / `list` / `show` / `update` / `search` は `--json` で機械可読な JSON を出力します（`delete` は対象外）。
- メモリを保存・更新・削除すると、目次 `MEMORY.md` と派生キャッシュ `index.json` が再生成されます。

```
$ usagi memory save --name "tabs" --title "ユーザーはタブを好む" --type user
saved tabs (user)

$ usagi memory list
user         tabs                     ユーザーはタブを好む
```

### `usagi mcp`

`usagi issue` / `usagi memory` と同じ issue・メモリ操作に加え、セッション操作（`session_create` / `session_list` / `session_prompt` / `session_pr` / `session_remove` / `session_delegate_issue`）を、**MCP（Model Context Protocol）サーバ**として AI エージェント（Claude Code など）に stdio 経由で公開します。issue・memory・session の tool を 1 つの `usagi` サーバが提供します。アーキテクチャ・対応 tool・`session_prompt` の挙動・JSON-RPC プロトコルの詳細は専用の章 [3.3 MCP サーバ](03-mcp.md) を参照してください。

### `usagi llm-mcp`

ローカル LLM（Ollama）を **MCP サーバ**として公開し、クラウド Agent が要約・命名・定型文生成などの軽量タスクを `local_llm_ask` ツールで委譲できるようにします。`--model` で委譲先モデルを指定します（既定は `qwen2.5-coder:7b`）。設定での有効化・資材のインストール・対応 tool の詳細は専用の章 [3.4 ローカル LLM MCP サーバ](04-llm-mcp.md) を参照してください。
