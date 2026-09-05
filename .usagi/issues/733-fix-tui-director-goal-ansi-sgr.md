---
number: 733
title: "fix(tui): Director の Goal 入力に ANSI SGR が可視表示される回帰を直す"
status: done
priority: high
labels: [v2, tui, fix, render]
dependson: []
related: [375]
created_at: 2026-09-05T00:00:00+00:00
updated_at: 2026-09-05T08:45:56.838224+00:00
---

## 問題

Director / Start Work Run の Goal 入力行に、ANSI SGR がそのまま可視文字として描かれる。
空 Goal では入力行が `[7m [0m` と表示され、block cursor が読めない。

原因は `crates/tui/src/presentation/views/director_drawer.rs` の `goal_composer_body` が、
**styled な文字列を plain-text 専用の折り返しへ渡している**ことである。

```rust
let caret = widgets::block_caret(goal, goal.len(), &Style::new());
let mut input = widgets::wrap_to_width(&caret, width) // styled を plain wrap に渡している
```

- `block_caret` は caret grapheme を `base.reverse().paint(..)` で反転するので、戻り値は
  `ESC[7m…ESC[0m` を含む。空 Goal では `MARKER + ESC[7m + " " + ESC[0m` になる。
- `widgets::wrap_to_width` は plain text 専用である。`presentation_character_is_safe` は制御文字である
  ESC だけを落とし、続く `[7m` / `[0m` は**通常の可視文字として残す**（`clip_to_width` /
  `display_width` と違い、エスケープを 0 桁として読み飛ばさない）。

結果として画面に `[7m [0m` が出る。`wrap_to_width` の他の呼び手（`create_session_error_modal`、
`workspace.rs` の inline session error、`decision_modal`、`welcome`、`open`、`mascot`）はすべて plain を
渡しており、**「plain を折り返してから行ごとに塗る」という契約は
[#375](375-fix-tui-inline-session-create-error-sidebar.md) が記録している**。
契約を破っているのは `goal_composer_body` の 1 か所だけである。

既存の view test は行を `strip_ansi` で正規化してから内容を assert していた。壊れた出力には ESC が
既に無く `[7m` は素の文字として残るため、`strip_ansi` は何も落とさず「Goal 文字列が含まれる」という
assert は壊れた行でも通り、回帰を検出できなかった。

## ゴール

Goal 入力行を、表示桁数どおりに折り返しつつ ANSI SGR を画面へ漏らさずに描く。

- 空 Goal・幅境界（caret の 1 桁が最終行に残らない幅）・CJK（全角 2 桁）・長文折り返しのいずれでも
  SGR が可視化されない。
- Goal 入力は末尾 caret だけを持つ（reducer は append / backspace のみで cursor を動かさない）。
  最終行が幅を使い切っているときは caret だけの行へ折り返し、caret を切り捨てない。
- `width == 0` でも caret 行 1 行を返し、既存の行数契約
  （`zero_width_goal_composer_keeps_its_row_contract`）を保つ。

## 変更内容

### 1. `widgets` に契約を持つ helper を置く

`crates/tui/src/presentation/widgets/mod.rs` に
`wrap_with_trailing_caret(value, width, base) -> Vec<String>` を追加する。plain な `value` を
`wrap_to_width` で折り返し、**行ごとに `base` で塗り**、最終行の末尾へ `block_caret` の block cursor を
置く。最終行が幅を使い切っているときは caret だけの行を足す。`wrap_to_width` は plain 専用のまま
据え置き、styled を渡してよい入口を作らない。

`block_caret` / `wrap_to_width` の doc に、両者を直接つなぐと SGR が可視化される旨と、この helper への
導線を書く。

### 2. `goal_composer_body` を helper へ載せ替える

`block_caret` → `wrap_to_width` の直結をやめ、`wrap_with_trailing_caret(goal, width, &Style::new())` を
使う。表示行数の窓（末尾 `available` 行）と footer の分岐は現状のまま。

## テスト

- `widgets`: 空文字 / 幅内 / 幅ちょうど（caret が次行へ落ちる）/ CJK 折返し / `width == 0` で、返る各行の
  表示桁数と、`strip_ansi` 後に SGR 断片が残らないことを固定する。
- `director_drawer`: 空 Goal・CJK Goal・狭幅で `drawer_body` の**生の行**を検査し、`[7m` / `[0m` のような
  SGR 断片が可視文字として現れないことを assert する（`strip_ansi` 済みの行だけを見る既存 test では
  検出できないため）。

## 非対象

- Goal 入力の cursor 移動（左右キー・単語移動）と grapheme 単位の backspace。reducer は現状 append /
  backspace のみで、本 issue は表示の回帰修正に限る。
- Goal が伸びたときの provider picker の行配分。`available` の算出は現状のままとする。
