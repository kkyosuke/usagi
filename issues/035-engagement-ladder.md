---
number: 033
feature: engagement-ladder
title: ホーム画面を 4 モード（統括・切替・在席・没入）の状態機械に再構成
status: done
priority: medium
category: tui
dependson: [027, 031]
---

# ホーム画面を 4 モード（統括・切替・在席・没入）の状態機械に再構成

## 概要

当初は「統括・在席・没入」を**画面内部の仕組み（`Mode` / `RightPane` / `CommandScope`）に名前を付けた語彙**として
定義し、コマンドスコープを**入れ子（nest）**にする方針だった。しかし実装を進めるなかで、これらを語彙ではなく
**実際の 4 つのモードと状態遷移**として組み直し、コマンドスコープの入れ子は**物理的な分離**へ戻すのが
分かりやすいと判断した。本 issue はその再構成の記録。

## 確定した 4 モードと状態遷移

`Ctrl-O` =「一段ズームアウト」、`Esc` =「一段戻る」。

```
起動 → 統括（Overview）
統括 --session switch（名前なし）--> 切替（Esc で統括へ戻る）
統括 --session switch <name>--> 在席(name)   （未知の名前 → 入力欄直下にエラー、統括のまま）
切替 --Enter/l--> ライブ? 没入 : 在席 ;  --c→インライン名入力→Enter--> 在席(新規) ;  --Esc/h--> 開いた元のモード ;  --Ctrl-O--> 統括
在席 --terminal/agent--> 没入 ;  --Esc--> 統括 ;  --Ctrl-O--> 切替
没入 --Ctrl-O--> 切替 ;  --Ctrl-O,Ctrl-O--> 統括 ;  --シェル終了--> 在席
```

- **統括（Overview）** — 既定モード。下部コマンドラインがワークスペース全体（`CommandScope::Workspace`：`session` /
  `config` / `doctor` ＋共通）を操作する。**結果・エラーは入力欄直下の帯**に出し、**右ペインは空白**。`Esc` で
  プロジェクト選択画面へ。
- **切替（Switch）** — セッションピッカー。**キーボードが左ペインそのものに移る**（中央モーダル・オーバーレイなし）。
  `↑↓`/`j`/`k` 移動、`Enter`/`l` 確定（ライブ→没入、アイドル→在席）、`c` で**左ペイン内のインライン名入力**から
  新規作成（空文字・重複をバリデーション）、`Esc`/`h` で元のモードへ、`Ctrl-O` で統括へ。
- **在席（Focus）** — 1 セッションを選んだ状態。**右ペイン**でそのセッションのコマンド（`CommandScope::Session`：
  `terminal` / `agent`、`ai` は coming soon）を操作する。右ペインの UI は設定 `session_action_ui`（`menu` / `prompt`）で選ぶ。
  `terminal` / `agent` → 没入、`Esc` → 統括、`Ctrl-O` → 切替。
- **没入（Attached）** — 埋め込みシェル / Agent がライブ動作（描画は従来どおり）。**`Ctrl-O` だけが予約キー**で
  `Esc` を含む他キーはシェルへ流れる。`Ctrl-O` で切替へ（もう一度で統括）、シェル終了で在席へ。

## やったこと（当初方針からの変更）

- **3 語の語彙 → 4 つの実モード**：統括・在席・没入を実際のモードへ昇格し、セッションピッカーとして **切替** を新設。
  `Ctrl-O`＝ズームアウト、`Esc`＝バックアウトで統一。`document/design/05-home.md` を語彙の説明から
  「モードと状態遷移」の状態機械へ書き換え（目次・レイアウト・ASCII モックアップ・キー操作表を含む）。
- **スコープの入れ子化を撤回 → 物理的に分離**：027/031 のスコープ入れ子（`CommandScope::visible_in` が
  Workspace を Session でも可視にする）を**廃止**し、`visible_in` を「同一スコープか `Both` か」のシンプル判定へ戻した。
  統括の下部ライン（Workspace）と在席の右ペイン（Session）は**別々の入力面**で、互いのコマンドは出ない。
- **モーダル / オーバーレイの削除**：中央の**セッション名入力モーダル（SessionModal）**と **`Ctrl-O` オーバーレイの
  セッションピッカー（SessionPicker）**を削除。新規作成は切替の左ペイン内インライン入力に、セッション切り替えは
  切替モードに置き換えた。**セッション削除モーダル（RemoveModal）は維持**。
- **新設定 `session_action_ui` を追加**：グローバル設定（`menu` 既定 / `prompt`）。Config 画面の「Agent CLI」と
  「Local LLM」の間に "Session Action UI" 行を追加し、CLI `usagi config` にも一覧表示。在席の右ペイン UI を選ぶ。

## 関連ドキュメント更新

- `document/design/05-home.md`：4 モードの状態機械として全面改稿（モード・状態遷移・レイアウト・キー操作・
  コマンドスコープ＝物理分離・切替のインライン作成・SessionModal/SessionPicker 記述の削除、RemoveModal は維持）。
- `document/03-commands/02-tui.md`：スコープを物理分離に更新。`session switch`（名前なし）→切替、`terminal` / `agent`
  は在席の右ペインから実行。`Ctrl-O` オーバーレイ記述を「切替へズームアウト」に置換。
- `document/05-settings.md` / `document/data/01-global.md`：`session_action_ui`（`menu` / `prompt`、既定 `menu`）を追加。
