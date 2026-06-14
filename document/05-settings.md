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
| 既定ワークスペース | `default_workspace` | string?\| | `null` | 既定で開くワークスペース名。未設定なら `null` |
| クローン先ベース | `workspace_root` | string?\| | `null`（→ `~/git`） | 新規プロジェクトのクローン先ベースディレクトリ。未設定時は `~/git` にフォールバック |
| デスクトップ通知 | `notifications_enabled` | bool | `true` | `hop` 時や、バックグラウンドの `agent` が入力待ちになった時などのデスクトップ通知の ON/OFF |
| Agent CLI | `agent_cli` | enum | `claude` | 起動する AI エージェント CLI（`claude` / `gemini`） |

> すべての項目はフォーマットバージョン `version: 1` とともに `settings.json` に格納されます。
> 完全な JSON 例は [data/01-global.md](data/01-global.md#settingsjson) を参照してください。

## ローカル設定（プロジェクト単位の上書き）

グローバル設定のうち **プロジェクトごとに変えたい項目だけ** を、各リポジトリの
`<repo>/.usagi/settings.json` で上書きできます（ローカル設定）。コミットされず、マシンごとに保持されます。

| 項目 | JSON キー | 型 | 未設定時 |
|---|---|---|---|
| Agent CLI | `agent_cli` | enum?\| | グローバル設定にフォールバック |
| デスクトップ通知 | `notifications_enabled` | bool?\| | グローバル設定にフォールバック |
| デフォルトブランチ基点 | `default_branch_source` | enum?\| | 既定（`remote`） |

> **デフォルトブランチ基点（`default_branch_source`）**: `session new` でセッションを作るとき、各 git
> リポジトリの worktree を切る新ブランチの**基点**を選びます。選択肢は `local`（ローカルの既定ブランチ。例
> `main`）と `remote`（リモート追従の既定ブランチ。例 `origin/main`）。グローバル設定に対応項目はなく、
> 未設定時は `remote` として扱います（`origin/<既定>` が無ければローカル既定ブランチ → それも無ければ現在の
> HEAD にフォールバック）。**リポジトリ単位**の設定なので、複数 git を含むワークスペースでは各リポジトリ内で
> Config を開いて個別に設定します。

- 全フィールドが任意（`Option`）で、`null` は「グローバル設定に従う」を意味します。テーマ（`theme`）や
  クローン先（`workspace_root`）のようにプロジェクト単位で変える意味の薄い項目は対象外です。
- **実効設定 = グローバル設定にローカルの上書きを適用した結果**。解決は `domain/settings.rs` の
  `Settings::with_local`、ユースケースは `usecase/settings.rs` の `effective(storage, repo_root)` が担います。
- 読み書きロジック・永続化（[issue 021](../issues/021-local-settings.md)）に加え、編集 UI も実装済み
  （[issue 022](../issues/022-local-settings-ui.md)）。git リポジトリ内で設定画面（Config）を開くと、グローバル
  設定の下に「Local · Agent CLI」「Local · Notifications」「Local · Default Branch」の行が現れ、Agent CLI と
  Notifications は **「グローバルに従う / ローカルで上書き」**、Default Branch は **`local` / `remote`** を
  切り替えられます。詳細は [design/04-config.md](design/04-config.md) を参照。
- JSON 例・フィールド詳細は [data/02-workspace.md](data/02-workspace.md#settingsjson-プロジェクト固有の設定上書きローカル設定) を参照。

## 設定の変更方法

### 設定画面（Config）

`usagi hop` の起動画面で `Config`（`c`）を選ぶか、ホーム画面のコマンドモードで `config` を実行すると
設定画面に入ります（ホーム画面から開いた場合は、起動中のワークスペースのローカル設定が編集対象になります）。

- 各項目は `< 値 >` の左右セレクタ。`↑↓` で項目移動、`←→` で値の切り替え。
- 変更はメモリ上に保持され、未保存の項目はラベル左の黄色 `●` と黄色の値で明示されます。
- 末尾の **Save ボタン**で `Enter` を押すと `settings.json` へ保存します（変更があるときだけ有効）。

操作の詳細・レイアウトは [design/04-config.md](design/04-config.md) を参照してください。

> 現状 Config 画面で編集できるのは Theme / Default Workspace / Notifications / Agent CLI の 4 項目です。
> `workspace_root` は `settings.json` に保存されますが、画面からの編集は今後対応予定です。

### CLI

CLI からも設定を確認・編集できます（[issue 015](../issues/015-config-edit.md)、[3. コマンドリファレンス](03-commands/README.md)）。

- `usagi config` — 現在のグローバル設定を `key  value` 形式で一覧表示。
- `usagi config --edit` — 設定ファイルを `$EDITOR`（→ `$VISUAL` → OS 既定）で開いて編集。保存後に
  再パースで形式（JSON 構文・必須 `version`・各フィールドの型）を検証し、不正なら編集前の内容へ
  巻き戻します。

## 環境変数

| 環境変数 | 役割 |
|---|---|
| `USAGI_HOME` | グローバルデータディレクトリ（`workspaces.json` / `settings.json` の置き場）を上書きする。未設定なら `~/.usagi` |

## 設定が効く場面

| 設定 | 効く場面 |
|---|---|
| `theme` | TUI 全体の配色 |
| `default_workspace` | 起動時に既定で開くワークスペースの選択 |
| `workspace_root` | 新規プロジェクト画面（Clone）の Location 既定値（[design/03-new.md](design/03-new.md)） |
| `notifications_enabled` | `hop` 時や、バックグラウンドの `agent` が入力待ちになった時などのデスクトップ通知の表示可否 |
| `agent_cli` | `agent` / `ai` コマンドが起動する AI エージェント CLI の選択（[4. オーケストレーション](04-orchestration.md)） |

> 設定の永続化は `usecase/settings.rs`（`load` / `save` / 各 `set_*`）と
> `infrastructure/storage.rs`（`Storage`）に実装されています。
