# 5. ホーム画面（Home）✅ 実装済み（3 ペイン + コマンドモード）

> [画面設計トップ](README.md) ｜ ← 前へ [4. 設定画面（Config）](04-config.md)

プロジェクト選択画面でワークスペースを選ぶと遷移する、ワークスペース操作のメイン画面。
worktree 一覧（左ペイン）・コマンド履歴/出力（右ペイン）・コマンド入力欄（下部）の 3 ペイン
構成で、**サイドバーモード**（worktree 一覧の操作）と**コマンドモード**（TUI 内コマンドの入力）を
切り替えて操作します。`terminal` コマンドを実行すると、右ペインは履歴/出力からライブの埋め込み
ターミナルに切り替わります（左ペインの一覧は表示したまま）。worktree の情報はワークスペースの `<workspace>/.usagi/state.json`
（画面外で `usagi` が同期した内容）から読み込みます。起動画面と同じ代替スクリーン上に描画されます。

> TUI 内コマンドの一覧・引数・実装状況は [../03-commands/02-tui.md](../03-commands/02-tui.md) に集約しています。
> 本書は画面レイアウトとモード・キー操作に絞ります。

## 目次

- [レイアウト](#レイアウト)
- [構成要素](#構成要素)
- [モードとキー操作](#モードとキー操作)
- [TUI 内コマンド](#tui-内コマンド)
- [コマンドレジストリ（拡張点）](#コマンドレジストリ拡張点)
- [セッション名入力モーダル](#セッション名入力モーダル)
- [履歴の永続化](#履歴の永続化)
- [読み込みと今後の予定](#読み込みと今後の予定)

## レイアウト

端末全体を使い、上部にタイトルバー、中央を縦の罫線 `│` で左右 2 ペインに分割、下部に
コマンド入力欄とフッターを配置します。左ペインの幅は端末幅の約 1/3（16〜40 桁にクランプ）です。

```text
┌──────────────────────────────────────────────────────────┐
│                  usagi · 3 worktrees                     │  ← タイトルバー（緑・太字、中央寄せ）
│                                                          │
│  > ● main       pushed  │ Type ":" to enter a command…   │  ┐ 左：worktree 一覧
│    feature/x    local   │ ❯ man                          │  │ 右：コマンド履歴/出力
│    fix/y        merged  │ Available commands:            │  │   （新しい行ほど下、溢れたら古い行から省略）
│                         │   session  Create or manage…   │  ┘
│                         │ Opening "main" is coming soon 🐰│
│ ❯ man▏                                                    │  ← コマンド入力欄（コマンドモード時）
│ Tab: complete / ↑↓: history / Enter: run / Esc: cancel    │  ← フッター（モード別・淡色）
└──────────────────────────────────────────────────────────┘
```

worktree の記録が無い（`state.json` 未生成など）場合は、左ペインに
`No worktrees recorded yet. Run usagi to sync.` を表示します。

`terminal` を実行している間は、右ペインが埋め込みターミナルに切り替わります。左ペインの worktree
一覧はそのまま、右ペインにシェルのライブ出力（疑似ターミナルの画面グリッド）を描画し、入力欄は
`● live terminal`、フッターは `Embedded terminal — Ctrl-O: detach and close` を表示します。

```text
┌──────────────────────────────────────────────────────────┐
│                  usagi · 3 worktrees                     │
│                                                          │
│  > ● main       pushed  │ $ cargo test                   │  ┐ 左：worktree 一覧（表示継続）
│    feature/x    local   │ running 42 tests…              │  │ 右：埋め込みターミナル
│    fix/y        merged  │ test result: ok. 42 passed     │  │   （シェルのライブ出力）
│                         │ $ ▏                            │  ┘
│ ● live terminal                                          │  ← 入力欄（ターミナル実行中）
│ Embedded terminal — Ctrl-O: detach and close             │  ← フッター
└──────────────────────────────────────────────────────────┘
```

## 構成要素

| 要素 | 内容 | スタイル |
|---|---|---|
| タイトルバー | `<ワークスペース名> · N worktree(s)` | 緑・太字（中央寄せ） |
| worktree 一覧（左ペイン） | 「カーソル + primary マーカー + ブランチ名(幅揃え) + status」で 1 行ずつ表示。ブランチ名は左ペイン幅に収まるよう末尾を省略 | 選択行：`>` 赤太字／primary：`●` マゼンタ／ブランチ名 シアン（選択行は太字）／status は色分け |
| status | ブランチの状態 `local` / `pushed` / `merged` | `local`：黄色／`pushed`：緑／`merged`：淡色 |
| コマンド履歴/出力（右ペイン） | 入力したコマンドのエコー（`❯` 付き）・出力・エラー・通知を時系列で表示。ペインに収まらない古い行は上から省略。**`terminal` 実行中はここが埋め込みターミナルに切り替わり**、シェルの画面グリッドを 1 行ずつ描画（実際のカーソルもシェルの位置に追従） | コマンド：シアン太字／出力：素／エラー：赤／通知：黄色 |
| コマンド入力欄 | コマンドモードでは `❯ <入力>▏`（プロンプト赤太字・入力シアン・末尾にキャレット）。サイドバーモードでは入り方のヒント。ターミナル実行中は `● live terminal`（緑） | コマンドモード：プロンプト赤太字・入力シアン／サイドバー：淡色／ターミナル：緑 |
| フッター | モード別・状態別の操作ヘルプ（ターミナル実行中は `Ctrl-O: detach` を案内） | 淡色（dim） |

## モードとキー操作

### サイドバーモード（既定）

| キー | 動作 |
|---|---|
| `↑` / `k` | worktree 選択を 1 つ上へ移動（先頭で押すと末尾へラップ） |
| `↓` / `j` | worktree 選択を 1 つ下へ移動（末尾で押すと先頭へラップ） |
| `Enter` | 選択中の worktree のセッションをアクティブに切り替える（`session switch` と同等） |
| `:` / `i` | コマンドモードへ切り替え |
| `q` / `Esc` | プロジェクト選択画面へ戻る |
| `Ctrl+C` | アプリを終了 |

### コマンドモード

| キー | 動作 |
|---|---|
| 文字キー | 入力欄に追記 |
| `Backspace` | 入力欄の末尾を 1 文字削除 |
| `Tab` | コマンド名を補完（一意なら確定、曖昧なら共通接頭辞まで補完し候補を右ペインに列挙） |
| `↑` / `↓` | コマンド履歴を遡る／戻る（末尾を越えると空行に戻る） |
| `Enter` | 入力を実行（右ペインにエコー＋出力を追記、履歴に記録） |
| `Esc` | 入力を破棄してサイドバーモードへ戻る |
| `Ctrl+C` | アプリを終了 |

### 埋め込みターミナル（`terminal` 実行中）

`terminal` を実行すると右ペインがライブシェルに切り替わり、キー入力はすべてシェルへ転送されます
（矢印・`Tab`・`Ctrl` 系などは対応するバイト列にエンコードして送出）。

| キー | 動作 |
|---|---|
| 任意のキー | そのままシェルへ転送（通常のターミナルとして操作） |
| `Ctrl+O` | デタッチしてシェルを閉じ、コマンドモードへ戻る |
| （シェルの `exit` 等） | シェル終了で自動的に右ペインがコマンド履歴/出力へ戻る |

## TUI 内コマンド

ホーム画面のコマンドモードで実行できる TUI 内コマンドの抜粋です（完全な仕様は
[../03-commands/02-tui.md](../03-commands/02-tui.md) を参照）。

| コマンド | 動作 | 状態 |
|---|---|---|
| `man` / `help` | `man` でコマンド一覧、`man <command>` で個別の書式（Usage）と例（Examples）を表示 | ✅ 実装済み |
| `history` | これまでに入力したコマンドの履歴を番号付きで表示（過去セッション分も含む） | ✅ 実装済み |
| `clear` | 右ペインの出力ログを消去 | ✅ 実装済み |
| `quit` / `exit` | アプリを終了 | ✅ 実装済み |
| `session` | `session new <name>` でセッション作成（`session new` と名前省略時は名前入力モーダル）。`session list` で一覧、`session switch <name>` でアクティブセッション切り替え（引数なしで一覧、worktree 一覧の Enter でも切り替え）、`session remove <name> [--force]` で削除（未コミット変更があれば警告し `--force` で破棄） | ✅ 実装済み |
| `terminal` | 選択中の worktree（未選択時はワークスペースルート）で対話型シェルを**右ペインに埋め込んで**起動。左ペインの一覧は表示したまま。`Ctrl-O` でデタッチ | ✅ 実装済み |
| `ai` / `doctor` | 認識はするが本体は未実装。「coming soon」を表示 | 🚧 プレースホルダー |
| 上記以外 | `unknown command: "…" (try "man")` を赤エラーで表示 | ✅ 実装済み |

`session switch` で選んだ **アクティブなセッション（worktree）** は左ペインで `*`（緑）マーカーと太字で強調
表示され、後続コマンド（`terminal` / `ai` など）の実行対象になります。キーボードのカーソル（`>`）と
アクティブ表示（`*`）は独立しています。`session switch <name>` は切り替えという状態変更を伴うため、
`Effect::Activate(name)` を返して画面側（`HomeState`）で解決します。

## コマンドレジストリ（拡張点）

TUI 内コマンドは `command.rs` の `Command` トレイトとして表現され、`CommandRegistry` に登録されます。
ディスパッチ・補完・`man` 一覧はすべてこのレジストリ経由で行われ、コマンドを `match` でハードコードしません。
各コマンドは説明（`description`）に加えて書式（`usage`）と例（`examples`）を `Command` トレイトで宣言でき、
`man <command>` がそれを自動的に表示します（未宣言ならコマンド名のみの既定の書式になります）。
後続コマンド（`ai` など）は **`Command` を実装して `register` するだけ**で
この基盤に乗ります（現状は「coming soon」プレースホルダーとして登録済み）。`session` / `terminal` は
この方式で実装済みの実例です。`session` は `new` / `list` / `switch` / `remove` のサブコマンドを持ち、
name 省略時は `Effect::OpenSessionModal` を返して event loop が名前入力モーダルを開きます。`terminal` は
`Effect::OpenTerminal` を返し、event loop が右ペインをターミナルモードに切り替えて埋め込みシェルを
起動します（PTY I/O と描画ループは `home/mod.rs` 経由で `home/terminal_pane.rs` が担当。画面グリッドの
スナップショットは純粋な `home/terminal_view.rs`、疑似ターミナル本体は `infrastructure/pty.rs`）。

## セッション名入力モーダル

`session new` を **名前なし** で実行すると、コマンドは `Effect::OpenSessionModal` を返し、
event loop がセッション名を入力するモーダルを画面中央に表示します。`session new <name>` のように名前を渡した
場合はモーダルを介さず直接作成します。

- `Enter` で入力を確定し、その名前でセッションを作成。`Esc` でキャンセルしてホーム画面へ戻る。
- 空文字や既存セッションと重複する名前はバリデーションし、モーダル内にエラーを表示して確定させない。
- 既存のモーダル基盤（`src/presentation/tui/widgets/` の `boxed` / `render_modal`）を再利用し、
  ディレクトリ選択モーダル（[03-new.md](03-new.md#ディレクトリ選択モーダル)）と同じく中央寄せ・枠付きボックスで描画する。
- モーダルの状態は `home/state.rs`、描画は `home/ui.rs`、キー処理は `home/event.rs` に実装。

> セッション作成そのもの（再帰走査・worktree 構築・コピー）の概念は [../04-orchestration.md](../04-orchestration.md) を参照。

## 履歴の永続化

実行したコマンドは `<repo>/.usagi/history.json` に 1 件ずつ追記され（`infrastructure/history_store.rs` の
`HistoryStore`）、次回の画面起動時に読み込まれて `history` コマンドと `↑`/`↓` 遡りに反映されます。
書き込みはベストエフォートで、失敗しても画面操作は止めません。詳細は
[../data/02-workspace.md](../data/02-workspace.md#historyjson) を参照。

## 読み込みと今後の予定

- worktree 状態の読み込みに失敗した場合は、空一覧の状態でエラー内容を右ペインに表示します。
- `session` はセッション作成（`.usagi/worktree/<name>/` への再帰的な worktree 構築・コピー）、
  名前入力モーダル、`session list`（state.json の `sessions` 一覧）、`session switch`（アクティブ
  セッションの切り替え）、`session remove`（worktree・ブランチ・コピーの削除、未コミット変更があれば
  警告し `--force` で破棄）を実装済み。worktree を選択したあとのセッション画面への遷移は今後の作業です。
- `terminal` は選択中の worktree（未選択時はワークスペースルート）で対話シェルを右ペインに埋め込んで
  起動します。シェルは `$SHELL`（未設定なら `bash`、Windows は `cmd.exe`）を疑似ターミナル（portable-pty）
  上で動かし、出力を vt100 でパースして画面グリッドを右ペインに描画します。`Ctrl-O` でデタッチ、または
  シェルの `exit` で右ペインがコマンド履歴/出力に戻ります。実体は起動シェルの解決が
  `infrastructure/terminal.rs`、PTY セッションが `infrastructure/pty.rs`、画面スナップショットが
  `home/terminal_view.rs`、描画/入力ループが `home/terminal_pane.rs`（`home/mod.rs` の `open_terminal`
  から起動）。
- `ai` / `doctor` の本体実装は今後の作業で実装します（Git 操作・AI 連携などのインフラ層に
  依存するため別途）。これらが司る worktree オーケストレーションの全体像は
  [../04-orchestration.md](../04-orchestration.md) を参照してください。

> 描画ロジックは `src/presentation/tui/home/`（画面状態 `state.rs`・描画 `ui.rs`・
> イベントループ `event.rs`・コマンドレジストリ `command.rs` に分離）に実装されています。
> worktree 一覧の元データ（`state.json`）の仕様は [../data/02-workspace.md](../data/02-workspace.md) を参照してください。
