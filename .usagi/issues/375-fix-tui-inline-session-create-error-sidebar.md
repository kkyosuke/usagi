---
number: 375
title: fix(tui): inline session create の error を sidebar 幅で折り返して表示する
status: done
priority: medium
labels: [tui, fix, ux, render]
dependson: []
related: [369, 370, 361]
created_at: 2026-07-19T22:33:19.561183+00:00
updated_at: 2026-07-19T22:48:17.192318+00:00
---

## 背景 / 問題

Home 左 sidebar の inline `+ new session` 入力（`workspace.rs::new_session_input_line`）は、
validation error を **caret 行と同じ 1 行に連結してから `clip_to_width` で末尾を `…` に切り詰めて**
表示している。

```rust
let mut line = format!("{} + new: {caret}", accent.paint(">"));
if let Some(error) = &draft.error {
    line = format!("{line}  {}", Role::Danger.style().bold().paint(error));
}
widgets::pad_to_width(&widgets::clip_to_width(&line, width), width)
```

このため:

- error 文が sidebar 幅を超えると **途中で切り捨てられ**、何が問題か読めない。安全に短縮した
  daemon 失敗 message や、CJK を含む長めの安全文では特に情報が落ちる。
- `document/03-tui.md` は「入力中は … error を**行の下に** error として表示する」と規定しているが、
  実装は同一行に載せており **doc と実装が乖離**している。
- error が実際に折り返して複数行になっても、viewport の scroll 起点を決める
  `home_row_height(Selection::NewSession)` は常に `1` を返す。`home_row_lines_at` が返す行数と
  `home_row_height` が **一致しない**ため、error が伸びると viewport / footer の高さ計算がずれる
  余地がある（現状は同一行 clip で 1 行に潰しているので顕在化していないだけ）。

なお既存の `widgets::wrap_to_width` は char 境界で折り返すが **ANSI エスケープを 0 桁扱いしない**
（`[31m` の各バイトを幅として数える）ため、スタイル済み文字列を直接渡すと桁が狂う。折り返しは
**plain な error 文に対して行い、行ごとに danger style を付与**する必要がある。

## ゴール

inline `+ new session` の validation / 安全な作成 error を、**sidebar 幅（`unicode-width` 準拠の
表示桁数）に合わせて caret 行の下へ正しく折り返して**表示する。

- CJK・ANSI style・極小幅・長い安全 error 文でも、行がはみ出し / 意図しない切り捨て / レイアウト
  崩れを起こさない。各折り返し行は表示幅 `width` に pad/clip して桁を揃える。
- 入力行（caret 行）＋折り返した error 行の合計行数と、`+ new session` row の高さ計算
  （viewport scroll 起点 / footer）を**構造的に一致**させる。
- 入力値（draft name）と再編集可能性は維持する。error は入力に付随する表示で、draft は失わない。
- raw protocol / internal / secret detail は表示しない（error 文は既存の safe message のまま。
  本 issue は生成ではなく**折り返し表示のみ**を扱う）。

## 変更内容（`crates/tui/src/presentation/views/workspace.rs`）

### 1. 純粋な行ビルダーを切り出す

`new_session_input_line`（単一 String を返す）を、**純粋で単体テスト可能な**行ビルダー
`new_session_input_lines(width, draft) -> Vec<String>` に置き換える。

- 先頭行: `> + new: <caret>`（従来どおり `clip_to_width` → `pad_to_width` で 1 行に収める）。
- error があるとき: **plain な `draft.error` を `widgets::wrap_to_width(error, width)` で折り返し**、
  各 segment を `Role::Danger` style で塗ってから `pad_to_width(_, width)` で桁を揃えて追加する。
  ANSI をラップに通さないため桁が狂わない。
- `width == 0` / 空 name / error なし の退避も破綻しない（先頭 1 行のみ）。

この関数は coverage 対象（coverage-on）とし、後述の pure test で全分岐を検証する。caret のスタイル
合成など IO を持たない presentation glue（`home_row_lines_at` 等）は従来どおり coverage-off のまま。

### 2. 高さ計算を行ビルダーへ一致させる

- `home_row_lines_at` の NewSession + draft 分岐を `new_session_input_lines(width, draft)` の結果を
  返すよう配線する（現在の単一行 push を置換）。
- viewport scroll 起点を決める `home_viewport_start` / `home_row_height` を **width・home 参照可能**に
  拡張し、NewSession + draft のときは `new_session_input_lines(width, draft).len()` を高さとして使う
  `home_row_height_at(width, home, row)` を導入する。これにより「描画行数 == 高さ計上」が構造的に
  保証され、error が複数行に伸びても viewport / footer がずれない。

## テスト（pure render / runtime regression）

`new_session_input_lines` の pure unit test:

- error なし → 1 行のみ（caret 行）。
- 短い error → caret 行 + 1 行の error。
- 長い安全 error → 複数行に折り返し、**元テキストが全て保持され切り捨てられない**（各行を strip して
  連結すると原文に一致）。
- CJK を含む error → 各行の `display_width` が `width` を超えない（全角 2 桁計上）。
- ANSI: 各 error 行が danger style を持ちつつ `display_width` が `width` 以下（style がラップに混ざって
  桁が狂わない）。
- 極小幅（`width` 1〜数桁）で panic せず、行が生成される。

render / height 整合の regression:

- draft error を折り返す状況で `home_left_pane` を描いても footer が 1 本だけ残り、viewport 行数が
  `height` を超えない（scroll 起点計算と描画行数の一致を確認）。
- 既存の `render_home_draws_a_create_validation_error_inline_on_the_new_session_row` /
  `render_home_draws_a_live_invalid_character_error_inline`（join 済み本文に error を含む）が引き続き green。

## ドキュメント

- `document/03-tui.md` の inline `+ new session` 節を、error を **行の下に幅折り返しで**表示する
  実装に合わせて明記する（同一行 clip ではない旨、狭幅時も `unicode-width` 準拠で折り返す旨）。

## 非スコープ

- 作成失敗 dialog（受付後の daemon 失敗）の表示（#369 で対応済み。dialog 側の 1 行縮約は変更しない）。
- validation / error message の生成ロジック、daemon 失敗の握り潰し（#370 で対応済み）。
- loading / skeleton の描画。
- caret 行（name 入力）自体の折り返し（name は 64 文字上限・自分の入力で、狭幅では従来どおり clip。
  draft 値は state に保持され再編集可能）。
