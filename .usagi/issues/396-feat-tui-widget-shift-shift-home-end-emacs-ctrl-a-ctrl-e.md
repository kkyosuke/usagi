---
number: 396
title: feat(tui): 入力 widget に範囲選択（Shift+←/→・Shift+Home/End）と emacs 行頭/行末移動（Ctrl+A/Ctrl+E）を追加する
status: done
priority: medium
labels: [tui, input, widget, ux]
dependson: []
related: [42, 257, 287]
created_at: 2026-07-20T04:25:28.639907+00:00
updated_at: 2026-07-20T06:46:29.158782+00:00
---

## 目的

TUI の 1 行入力欄（session 作成名・Open filter・Overview/Closeup palette など）で、次の編集操作をできるようにする。

1. **範囲選択**: `Shift`+`←` / `Shift`+`→` でキャレット位置から 1 文字ずつ連続選択、`Shift`+`Home` / `Shift`+`End` で行頭 / 行末までを一括選択する。
2. **emacs 行頭/行末移動**: `Ctrl+A` で入力の先頭へ、`Ctrl+E` で末尾へキャレットを移す。
3. 選択中の文字入力・`Backspace`・`Delete` は選択範囲を置換 / 削除し、それ以外のキャレット移動・`Esc` は選択を解除する。

共通 widget [`TextInput`](../../crates/tui/src/presentation/widgets/text_input.rs) にこの振る舞いを載せ、`TextInput` を使うすべての入力欄へ一貫して効かせる。

## 背景・調査結果

### 共通 widget は `TextInput`

`crates/tui/src/presentation/widgets/text_input.rs` が端末非依存の 1 行編集バッファ（`value` + `char` 境界に乗る `cursor`）で、`insert` / `backspace` / `delete_forward` / `move_left` / `move_right` / `move_home` / `move_end` を持つ。マルチバイト（日本語）は `prev_boundary` / `next_boundary` で 1 文字単位に扱い、文字の途中に落ちない。`#42`（done）でこのキャレット移動が入った。**選択状態は未実装**。

描画は `widgets/mod.rs` の `block_caret(value, cursor, base)` がキャレット直後の 1 セルを reverse-video で反転する。**範囲選択のハイライトは未対応**。

### キー変換経路は 2 系統あり、どちらも modifier を落としている

管理画面の入力は 2 つの語彙へ変換される。実装ではこの両方を通す必要がある。

| 経路 | 変換関数 | 消費側 |
|---|---|---|
| legacy `Key`（`usecase/application.rs`） | `src/runtime/tui.rs` の `passthrough_key` / `control_key` | `presentation/workspace_runtime.rs`・`views/new.rs`・`views/open.rs` |
| `AppKey`（`usecase/application/controller.rs`） | 同ファイルの `classify_management_input` | Home reducer |

現状の欠落:

- `passthrough_key`（`src/runtime/tui.rs:1077`）は `shift`+`Char` 以外の非既定 modifier を `Key::Passthrough(bytes)` に落とす。**`Shift`+`←`/`→` は入力欄へ届かない**。`KeyCode::Home` / `End` / `Delete` は `Key::Other` に落ちて未マッピング。
- `classify_management_input`（`controller.rs:1171`）は `KeyCode::Home` を `AppKey::CtrlA` に写す一方、`Shift` 付き矢印・`End`・`Delete` を扱わない。
- crossterm adapter `adapt_key`（`src/tui_input.rs:167`）は `Shift`/`Ctrl` の modifier を保持しており、**変換の下流だけ拡張すれば modifier は取り出せる**（adapter 自体は変更不要の見込み）。
- legacy `Key` enum には `Home` / `End` / `Delete` / 選択拡張のバリアントが無い。`AppKey` にも同様。

### `Ctrl+A` は既にグローバル予約 — 文脈依存で解決する（設計判断）

`Ctrl+A` は `+ new session` に予約済み（`src/runtime/tui.rs:1106`・`controller.rs:1199` の両経路、`document/03-tui.md:99` / `:155`）。`#287`（todo）は「create form 中の `Home`/`Ctrl+A` は caret 操作または no-op、フォームを再 submit しない」と規定しており、**文脈依存の `Ctrl+A` を既定方針としている**。

本 issue の解決（triage 決定）:

- **編集可能な `TextInput` にフォーカスがある間だけ** `Ctrl+A`=行頭 caret・`Ctrl+E`=行末 caret とする（emacs 動作）。
- フォーカスが無い navigation / Switch 文脈では従来どおり `Ctrl+A`=`+ new session` を維持する。
- これにより sibling session `triage-session-colors-ctrla`（Switch/session 切替の `Ctrl+A` を扱う）と**衝突・意味の混同を起こさない**。両者の境界は「テキスト入力にフォーカスがあるか」で切り分ける。

## スコープ

### 含める

- `TextInput` に選択アンカー（`anchor: Option<usize>`）を持たせ、`char` 境界安全な選択拡張 / 置換 / 削除 / 解除を実装する。
  - 選択拡張: `select_left` / `select_right`（1 文字）・`select_home` / `select_end`（端まで）。いずれもアンカー未設定なら現在キャレットにアンカーを立ててから移動する。
  - 非選択移動（`move_left/right/home/end`）・`Esc` 相当は選択を解除する。
  - `insert` / `backspace` / `delete_forward` は選択があれば**まず選択範囲を削除**してから通常動作（文字入力なら削除後に挿入）。空選択は解除のみ。
  - `selection()` → `Option<(usize, usize)>`（正規化した start..end のバイト範囲）と、描画に必要な分割アクセサを公開する。
- 選択ハイライトの描画: `widgets/mod.rs` に `block_caret` と同経路の選択対応レンダラ（例 `caret_with_selection` か `block_caret` の拡張）を用意し、選択範囲を reverse / accent 背景で塗る。既存の非選択描画・`INPUT_CURSOR_MARKER`・全角 2 桁計上（`unicode-width`）を壊さない。ANSI は 0 桁のまま。
- 2 系統のキー変換経路を拡張し、フォーカス中入力欄へ次を届ける:
  - `Shift`+`←`/`→`/`Home`/`End` → 選択拡張。
  - `Ctrl+A`/`Ctrl+E` → 行頭 / 行末 caret（フォーカス文脈のみ）。`Ctrl+E` は `End` と等価。
  - `Delete`（前方削除）・`Home`/`End`（非選択移動）が両経路で入力欄に届くこと。
  - 必要な `Key` / `AppKey` バリアント追加（例 `SelectLeft/Right/Home/End`・`LineStart/LineEnd`・`Home`/`End`/`Delete`）と、`passthrough_key` / `classify_management_input` / views・reducer の dispatch を接続する。
- 選択中の入力・削除・解除の挙動を既存 UX（`new` 作成フォームの inline 入力、`open` filter、palette）へ一貫適用する。

### 含めない

- PTY / live terminal 出力側のドラッグ選択・コピー（`#390` 系。`TerminalSession` / shell が所有）。本 issue は**入力 widget のテキスト選択のみ**。
- OS クリップボードへの選択コピー / 貼り付け（`copy_text` port の拡張）。将来別 issue。
- 複数行入力・折り返し編集。`TextInput` は 1 行のまま。
- navigation / Switch 文脈の `Ctrl+A`=`+ new session` の意味変更（`#287` / sibling session の担当領域を触らない）。
- profile/model UX・mouse による入力欄内選択。

## 既存 UX との整合（受け入れ観点）

- 選択があるとき文字を打つと選択が消えてその文字に置換される。`Backspace`/`Delete` は選択全体を消しキャレットは削除位置。
- 選択を保ったまま `←`/`→`/`Home`/`End`（Shift なし）を押すと、選択は解除されキャレットだけ移動する。`Esc` は入力欄をキャンセルする既存動作を保ちつつ、選択解除も自然に見える経路にする（`new` フォームの Esc=取消契約は維持）。
- CJK / 全角を含む選択でも byte 境界を割らず、ハイライト幅が見た目とずれない。
- inline `+ new session` 入力の緑 affordance・error 折り返し・skeleton 等、`document/03-tui.md` に記載の既存表示契約を回帰させない。

## 回帰テスト（必須）

- `TextInput` 単体: 選択拡張（左右・端）・アンカー起点・選択置換 insert・選択 backspace/delete・非選択移動での解除・空選択・CJK 境界での選択範囲、`selection()` の正規化。
- 描画: 選択ハイライトの `display_width` 一致（ASCII / CJK）・非選択時に既存 `block_caret` 出力と一致・行末 / 空文字での挙動。
- キー変換の modifier 組合せ回帰:
  - `src/tui_input.rs` の `adapt_key`（modifier 保持）。
  - `src/runtime/tui.rs` の `passthrough_key`: `Shift`+`←`/`→`/`Home`/`End`・`Ctrl+A`/`Ctrl+E`・`Delete` が期待どおりの `Key` になり、`Shift`+`Char` 等の既存分岐を壊さないこと。
  - `controller.rs` の `classify_management_input`: 同じ modifier 組合せが期待 `AppKey` になること。`Home`→行頭・navigation 文脈の `Ctrl+A`=new-session が維持されること。
  - フォーカス文脈による `Ctrl+A` の分岐（入力中=行頭 / navigation=new-session）。
- カバレッジ 100% を維持（`#[coverage(off)]` の新規追加は実 IO に限る。ロジックはテストで覆う）。

## ドキュメント

- `document/03-tui.md` の入力・キーバインド記述（`Ctrl-A` semantics 周辺 `:99`/`:116`/`:127` 表、inline 入力 `:149`〜）へ、範囲選択（`Shift`+矢印・`Shift`+`Home/End`）と emacs 行頭/行末（`Ctrl+A`/`Ctrl+E`、フォーカス文脈依存）を追記する。「記載＝実装済み」に従い、実装した挙動だけを現在形で書く。
- ユーザー向けに `Ctrl+A`/`Ctrl+E` の文脈依存（入力中=caret / navigation=new session）を明記し、混同を避ける。

## 完了条件

- session 作成入力・Open filter・palette など `TextInput` を使う各入力欄で、`Shift`+`←`/`→`/`Home`/`End` の連続 / 一括選択、`Ctrl+A`/`Ctrl+E` の行頭 / 行末移動ができる。
- 選択中の文字入力・`Backspace`/`Delete` が選択範囲を置換 / 削除し、非選択移動 / `Esc` が選択を解除する。
- 日本語 / 全角を含む選択・置換で文字が壊れない。
- navigation / Switch 文脈の `Ctrl+A`=`+ new session` は不変で、sibling session と衝突しない。
- modifier 組合せを含む回帰テストを追加し、CI（fmt / clippy / full test / coverage 100% / markdown-link-check）が green。
- `document/03-tui.md` を実装に合わせて更新。

## 関連

- `#42` 入力フィールドのカーソル移動（done。本 issue の基盤）。
- `#287` Switch の `Ctrl+A` から `+ new session`（todo。navigation 文脈の `Ctrl+A` 契約。本 issue は境界を尊重し触らない）。
- `#257` Home の `Ctrl+A` session lifecycle。
- sibling session `triage-session-colors-ctrla`（Switch/session 切替の `Ctrl+A`）。**編集フォーカスの有無で意味を切り分けて衝突回避**。
