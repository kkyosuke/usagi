# 5. 設定

> [ドキュメント目次](README.md) ｜ ← 前へ [4. オーケストレーション](04-orchestration.md) ｜ 次へ → [6. 開発規約](06-conventions.md)

`usagi` の設定の全体像をまとめます。本書は「どんな設定があり、どこに保存され、どう変えるか」という
機能視点のガイドです。保存フォーマットの詳細は [data/01-global.md](data/01-global.md)、設定画面の
見た目・操作は [design/04-config.md](design/04-config.md) を参照してください。

## 目次

- [設定の保存場所](#設定の保存場所)
- [設定項目](#設定項目)
- [ローカル設定（プロジェクト単位の上書き）](#ローカル設定プロジェクト単位の上書き)
- [設定の変更方法](#設定の変更方法)
- [環境変数](#環境変数)
- [設定が効く場面](#設定が効く場面)

## 設定の保存場所

アプリ全体の設定は、グローバルなデータディレクトリ配下の `settings.json` に保存されます。

```
~/.usagi/settings.json        # USAGI_HOME が設定されていればそのディレクトリ配下
```

- 解決順は ① 環境変数 `USAGI_HOME` → ② `~/.usagi`（`infrastructure/storage.rs` の `data_dir()`）。
- アトミック書き込み（`*.tmp` → `rename`）で保存され、書き込み途中のクラッシュでも壊れません。
- ファイルが無い初回起動時は、すべて既定値として扱われます。

## 設定項目

| 項目 | JSON キー | 型 | 既定値 | 選択肢 / 意味 |
|---|---|---|---|---|
| テーマ | `theme` | enum | `system` | `light` / `dark` / `system`（OS 追従）の UI カラーテーマ |
| 既定ワークスペース | `default_workspace` | string? | `null` | 既定で開くワークスペース名。未設定なら `null` |
| クローン先ベース | `workspace_root` | string? | `null`（→ `~/git`） | 新規プロジェクトのクローン先ベースディレクトリ。未設定時は `~/git` にフォールバック |
| デスクトップ通知 | `notifications_enabled` | bool | `true` | バックグラウンドの `agent` が入力待ち・完了になった時のデスクトップ通知の ON/OFF |
| Agent CLI | `agent_cli` | enum | `claude` | 起動する AI エージェント CLI（`claude` / `codex` / `codex_fugu` / `gemini`）。`codex_fugu` は Codex 互換 CLI で `codex-fugu` を起動する |
| セッションアクション UI | `session_action_ui` | enum | `menu` | ホーム画面の[在席](design/05-home.md#在席focus)で右ペインに出すアクション UI のスタイル。`menu`（選べるリスト）/ `prompt`（セッションスコープのコマンドライン） |
| サイドバー | `sidebar` | enum | `full` | ホーム画面の左セッション一覧を開く初期状態。`full`（全幅の一覧）/ `rail`（幅 5 桁に畳んだレール）。実行時は `Ctrl-B` で随時切り替えられる（[サイドバーの開閉](design/05-home.md#サイドバーの開閉ctrl-b)） |
| ローカル LLM 有効化 | `local_llm.enabled` | bool | `false` | 有効にすると `agent` 起動時に [ローカル LLM MCP サーバ](03-commands/04-llm-mcp.md)（`usagi-llm`）を wire し、軽量タスクをローカル LLM に委譲できる |
| ローカル LLM モデル | `local_llm.model` | string | `qwen2.5-coder:7b` | 委譲先の Ollama モデル名（`qwen2.5-coder:7b` / `:3b` / `:1.5b` / `qwen2.5:7b`） |

> ローカル LLM は **オプトイン**（既定 `false`）です。資材は Config 画面で **2 段階**に導入します:
> まず `Local LLM` 行の Install アクション（`Space` / `Enter` でモーダルを開き sudo パスワードを入力 →
> `ollama` ランタイムを導入）、次に `Local LLM Model` 行のモデル選択モーダル（一覧から選び、未導入のモデルは
> その場で `ollama pull`）。いずれも**バックグラウンドで進み、導入中も usagi の他機能を操作できます**（進行は
> 全画面共通の[ローディングうさぎ](design/04-config.md#インストール中のローディングうさぎ)で表示）。
> `usagi doctor --fix` はランタイムと既定モデルをまとめて導入します。詳細は
> [Config 画面のローカル LLM 導入](design/04-config.md) / [3.4 ローカル LLM MCP サーバ](03-commands/04-llm-mcp.md)。

> **セキュリティ注記**:
> - ランタイム導入は ollama 公式の `curl -fsSL https://ollama.com/install.sh | sh` を **sudo 権限で** 実行します。
>   HTTPS で取得しますが、取得スクリプトの内容に対する usagi 側のチェックサム/署名検証はありません（上流の手順に準拠）。
>   ollama.com / CDN / DNS が侵害された場合は任意コードが実行され得る点に留意してください。
> - `local_llm.model` は上表の allowlist の値のみ有効です。`settings.json` を手編集して allowlist 外の値を入れた場合、
>   ロード時に既定（`qwen2.5-coder:7b`）へ戻されます（model 名はエージェント起動コマンドに展開されるため）。

> すべての項目はフォーマットバージョン `version: 1` とともに `settings.json` に格納されます。
> 完全な JSON 例は [data/01-global.md](data/01-global.md#settingsjson) を参照してください。

## ローカル設定（プロジェクト単位の上書き）

グローバル設定のうち **プロジェクトごとに変えたい項目だけ** を、各リポジトリの
`<repo>/.usagi/settings.json` で上書きできます（ローカル設定）。コミットされず、マシンごとに保持されます。

| 項目 | JSON キー | 型 | 未設定時 |
|---|---|---|---|
| Agent CLI | `agent_cli` | enum? | グローバル設定にフォールバック |
| デスクトップ通知 | `notifications_enabled` | bool? | グローバル設定にフォールバック |
| デフォルトブランチ | `default_branch` | string? | リポジトリの検出済み既定ブランチ（auto） |
| デフォルトブランチ基点 | `default_branch_source` | enum? | 既定（`remote`） |
| ローカル LLM 有効化 | `local_llm_enabled` | bool? | グローバル設定（`local_llm.enabled`）にフォールバック |

> **デフォルトブランチ（`default_branch`）**: `session create` でセッションを作るとき、各 git リポジトリの
> worktree を切る新ブランチを**どのブランチから**切るかを選びます。未設定（`null` = auto）ならリポジトリの
> 検出済み既定ブランチ（`origin/HEAD` → 現在のブランチ → `main` の順で解決）を使い、`develop` のように
> 値を指定するとそのブランチを基点にします。Config 画面では対象リポジトリの**実在ブランチ**を検出して
> 選択肢に並べます（`auto` ＋ 各ブランチ名）。

> **デフォルトブランチ基点（`default_branch_source`）**: 上で選んだブランチを**ローカル形・リモート形の
> どちらで**基点にするかを選びます。選択肢は `local`（ローカルのブランチ。例 `develop`）と `remote`（リモート
> 追従のブランチ。例 `origin/develop`）。グローバル設定に対応項目はなく、未設定時は `remote` として扱います
> （`origin/<ブランチ>` が無ければローカルのブランチ → それも無ければ現在の HEAD にフォールバック）。
> いずれもワークスペースのローカル設定（`<workspace>/.usagi/settings.json`）に保存され、ホーム画面の
> `config` から編集します。

- 全フィールドが任意（`Option`）で、`null` は「グローバル設定に従う」を意味します。テーマ（`theme`）や
  クローン先（`workspace_root`）のようにプロジェクト単位で変える意味の薄い項目は対象外です。
- **実効設定 = グローバル設定にローカルの上書きを適用した結果**。解決は `domain/settings.rs` の
  `Settings::with_local`、ユースケースは `usecase/settings.rs` の `effective(storage, repo_root)` が担います。
- 読み書きロジック・永続化に加え、編集 UI も実装済みです。ホーム画面のコマンドモードで `config` を実行すると
  設定画面が**ワークスペーススコープ**で開き、「Agent CLI」「Notifications」「Default Branch」「Branch Source」
  の 4 項目を編集できます。Agent CLI と Notifications は **「グローバルに従う / ローカルで上書き」**、Default
  Branch は **`auto`（検出済み既定）／ リポジトリの各ブランチ**、Branch Source は **`local` / `remote`** を
  切り替えられます。詳細は [design/04-config.md](design/04-config.md) を参照。
- JSON 例・フィールド詳細は [data/02-workspace.md](data/02-workspace.md#settingsjson-プロジェクト固有の設定上書きローカル設定) を参照。

## 設定の変更方法

### 設定画面（Config）

設定画面は **開いた場所でスコープが分かれます**。

- `usagi hop` の起動画面で `Config`（`c`）を選ぶ → **グローバルスコープ**。アプリ全体の設定
  （`~/.usagi/settings.json`）を編集します。
- ホーム画面のコマンドモードで `config` を実行する → **ワークスペーススコープ**。起動中のワークスペースの
  ローカル設定（`<workspace>/.usagi/settings.json`）だけを編集します。

どちらのスコープも操作は共通です。

- 各項目は `< 値 >` の左右セレクタ。`↑↓` で項目移動、`←→` で値の切り替え。
- 変更はメモリ上に保持され、未保存の項目はラベル左の黄色 `●` と黄色の値で明示されます。
- 末尾の **Save ボタン**で `Enter` を押すとそのスコープの `settings.json` へ保存します（変更があるときだけ有効）。

操作の詳細・レイアウトは [design/04-config.md](design/04-config.md) を参照してください。

> グローバルスコープで編集できるのは Theme / Default Workspace / Notifications / Agent CLI /
> Session Action UI（「Agent CLI」と「Local LLM」の間）の各項目、ワークスペーススコープで編集できるのは
> Agent CLI / Notifications / Default Branch / Branch Source の 4 項目です。
> `workspace_root` は `settings.json` に保存されますが、設定画面では編集せず、`usagi config --edit` で変更します。

### CLI

CLI からも設定を確認・編集できます（[3. コマンドリファレンス](03-commands/README.md)）。

- `usagi config` — 現在のグローバル設定を `key  value` 形式で一覧表示。
- `usagi config --edit` — 設定ファイルを `$EDITOR`（→ `$VISUAL` → OS 既定）で開いて編集。保存後に
  再パースで形式（JSON 構文・必須 `version`・各フィールドの型）を検証し、不正なら編集前の内容へ
  巻き戻します。

## 環境変数

| 環境変数 | 役割 |
|---|---|
| `USAGI_HOME` | グローバルデータディレクトリ（`workspaces.json` / `settings.json` の置き場）を上書きする。未設定なら `~/.usagi` |
| `USAGI_TRACE` | 操作トレース（`logs/trace-YYYY-MM-DD.jsonl`）の記録を有効化する。空でも `0` でもない値で ON、未設定なら OFF（[data/01-global.md#logs操作トレース](data/01-global.md#logs操作トレース)） |

## 設定が効く場面

| 設定 | 効く場面 |
|---|---|
| `theme` | TUI 全体の配色 |
| `default_workspace` | 起動時に既定で開くワークスペースの選択 |
| `workspace_root` | 新規プロジェクト画面（Clone）の Location 既定値（[design/03-new.md](design/03-new.md)） |
| `notifications_enabled` | バックグラウンドの `agent` が入力待ち・完了になった時のデスクトップ通知の表示可否 |
| `agent_cli` | `agent` コマンドが起動する AI エージェント CLI の選択（[4. オーケストレーション](04-orchestration.md)） |
| `session_action_ui` | ホーム画面の[在席](design/05-home.md#在席focus)で右ペインに出すアクション UI（`menu` / `prompt`）の選択 |
| `sidebar` | ホーム画面の左セッション一覧を開く初期状態（`full` / `rail`）。実行時は `Ctrl-B` で切り替え（[サイドバーの開閉](design/05-home.md#サイドバーの開閉ctrl-b)） |
| `local_llm.enabled` / `local_llm.model` | 有効時、`agent` 起動コマンドに `usagi-llm` MCP サーバを追加し、軽量タスクをローカル LLM に委譲する（[3.4 ローカル LLM MCP サーバ](03-commands/04-llm-mcp.md)） |

> ホーム画面の `config` で `session_action_ui` を変更すると、設定画面を閉じてホームに戻った時点で
> 実効設定を読み直し、[在席](design/05-home.md#在席focus)の右ペインに反映します（ホーム画面を開き直す必要はありません）。

> 設定の永続化は `usecase/settings.rs`（`load` / `save` / 各 `set_*`）と
> `infrastructure/storage.rs`（`Storage`）に実装されています。
