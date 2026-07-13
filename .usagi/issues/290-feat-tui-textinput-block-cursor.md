---
number: 290
title: feat(tui): 共通 TextInput を block cursor 表示に統一する
status: todo
priority: high
labels: [tui, input, accessibility, parity]
dependson: [258]
related: [269, 278, 287, 288]
created_at: 2026-07-13T12:09:53.951585+00:00
updated_at: 2026-07-13T12:09:53.951585+00:00
---

## 目的

v2 TUI のフォーカス中で編集可能な 1 行入力を、下線キャレットから v1 と同じ **block cursor** に統一する。対象は Closeup の Prompt、Switch の検索/入力、session 名を含む New/Create/Rename 等、共通 `TextInput` を使うすべての編集欄である。

block cursor は入力位置の文字を反転表示し、行末・空欄では反転空白 1 セルを表示する。文字を追加の glyph で押し出さず、全角・Unicode・横スクロール/clip 後も位置と幅を保つ。読み取り専用値、非フォーカス値、候補選択/selection highlight の既存意味は変えない。

## 調査根拠

- 現行 `TextInput` は value と char-boundary の byte cursor を一元管理するが、描画は `new.rs` の `caret_text`、`open.rs` の `filter_line`、`overview_modal.rs` の `input_line`、`closeup_modal.rs` の prompt として分散している。前 3 者は accent 下線、Closeup Prompt は placeholder `_` であり、共通 widget を使う入力で表示契約が一致していない。
- v1 の `widgets::block_caret` は caret 直後の scalar を reverse-video で描き、末尾は reverse 空白にする。ゼロ幅 marker を frame painter が除去しつつ表示幅から実カーソル位置を求めるため、CJK 幅・clip・IME preedit を壊さない。
- v2 の `Style` は underline (SGR 4) までしか属性を持たず、`Frame::from_lines` / terminal adapter も入力位置を frame から渡す seam を持たない。単に各 view の underline を置換すると、表現の再分散と IME 位置ずれを残す。
- #258 が Home/Switch/Closeup の runtime と renderer を唯一の source に統合中である。同じ input owner/描画経路を並行して変更しないため、本 issue は #258 の後にその seam を消費する。#269/#278 の mode-aware key/prefix 契約は維持し、#287/#288 の Switch row work と重複しない。

## 実装方針

- `presentation::widgets` に、`TextInput` または before/after と base `Style` を受ける唯一の block-caret renderer を置く。通常文字は base style、caret cell は同じ semantic color の reverse-video (SGR 7) とし、最初の Unicode scalar 全体を反転する。空・末尾は反転空白にする。
- その renderer をフォーカス中の editable input だけから呼び、New/Open/Overview modal/Closeup Prompt と #258 後の Switch/create/rename surface を同一 API に寄せる。非フォーカス/読み取り専用の値・placeholder、候補の selected row、selection highlight の既存 style precedence を保つ。
- v1 の zero-width caret marker と同等の internal contract を v2 frame/terminal adapter に導入する。frame assembly・ANSI width/clip は marker を 0 幅として扱い、実端末へ出す前に全 marker を除去する。最初の marker の row/表示列を terminal cursor placement に渡し、IME preedit が入力欄に出るようにする。複数 marker は防御的に除去し、フォーカス中 editable input 以外は marker を持たない。
- `Style` の reverse 属性と ANSI parser/active-style/diff の扱いを追加し、incremental repaint で reverse/video/reset が後続セル、modal border、横スクロール外へ漏れないようにする。テーマは既存の Accent/Danger 等の semantic role を再利用し、新しい固定色を導入しない。
- 既存の `TextInput` の char-boundary 編集、キー分類、候補表示、focus transition、IME/key input ownership を変更しない。横スクロールが renderer 側に存在する場合は caret marker が clip 後も caret cell と同じ表示列へ残るようにする。

## 対象外

- `TextInput` の編集操作、キー binding、IME event decode、候補アルゴリズム、session lifecycle/daemon/PTY の仕様変更。
- non-editable label・terminal pane の hardware cursor・既存の link/tab/選択用途の underline の変更。
- #258 の Home runtime 統合、#287 の create lifecycle、#288 の current/cursor row marker の再実装。
- v1 のコードや退避仕様ドキュメントの更新。

## 完了条件

- Closeup Prompt、Switch の検索/入力、session 名を含む共通 `TextInput` 利用欄で、フォーカス中の挿入位置が reverse-video の block cursor として同じ規則で表示される。
- 空欄、先頭・中間・末尾、ASCII、結合文字を含む Unicode、全角 CJK、狭幅/clip/横スクロールで、caret は scalar を分割せず、可視文字列を不必要に横へずらさず、表示幅と枠を壊さない。
- frame/diff と terminal adapter は marker を端末出力へ漏らさず、表示幅で正しい caret column に実カーソルを置く。IME preedit と通常のキー入力が既存の入力欄に維持される。
- 非フォーカス・読み取り専用・候補/selection highlight の既存意図は維持され、Closeup/Switch の prefix、focus、candidate 操作、session create/rename の既存回帰を壊さない。
- 下線による入力キャレットの個別実装は削除し、共通 API 以外に block-caret 表現を複製しない。テーマは semantic role と accessibility を維持し、reverse cell は reset される。

## テスト

- widget unit: 空、先頭/中間/末尾、ASCII/CJK/combining scalar、reverse SGR、表示幅、clip/pad、style reset、marker の位置・0 幅・除去を固定する。
- frame/terminal adapter: marker を含む ANSI/CJK frame の row/column 計算、wide-cell 境界、incremental diff、複数 marker の防御的除去、IME cursor placement request を fake terminal で検証する。
- view/render snapshot: New/Open/Overview/Closeup と #258 統合後の Switch/create/rename を、focus/non-focus、candidate/selection、empty/end/mid cursor、narrow geometry で固定する。
- input regression: 既存の char-boundary 編集、candidate selection、Closeup/Switch prefix/focus、IME/キー入力 ownership、session 名入力の既存テストを維持する。
