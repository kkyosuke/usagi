---
number: 767
title: fix(tui): Open 画面に登録済み workspace が全件出ないのを直す
status: done
priority: high
labels: [v2, tui, open, regression]
dependson: []
related: []
created_at: 2026-09-17T00:00:00+00:00
updated_at: 2026-09-17T00:00:00+00:00
---

## 問題

Welcome → Open の一覧に、`usagi open <path>` で登録した workspace が出ない。登録自体は成功していて
`~/.usagi/workspaces.json` には entry がある（実機では 15 件登録されているのに 3 件しか出ない）。

原因は 2 つある。

1. **Open 一覧の供給元が Welcome の recent カード枠で切られている**。`open_from_registry` は
   `welcome.recent()` を読むが、この accessor は Welcome 右カラムのカード枚数（`RECENT_SLOTS = 3`、
   番号キー `1`〜`3`）に切った slice を返す。registry の生値は `open_overviews.is_empty()` のとき
   （＝登録が 1 件も無いとき）しか使われないため、**登録が 1 件でもあれば一覧は最大 3 件になる**。
   `feat(tui): enrich open workspace list (#918)` が `Open::new(workspaces)` を
   `open_from_registry(workspaces, welcome.recent())` へ置き換えたときに入った退行である。

2. **一覧に viewport が無く、端末に収まらない行は黙って捨てられる**。`body_lines` は filter 後の
   全件を 1 件 2 行で並べるだけで、`Frame::from_lines` が `take(height)` するため溢れた行は消える
   （フッタごと消える）。15 件では 44 行必要で、標準的な端末高さでは末尾が届かない。1 の修正だけでは
   「一覧には載るが画面に出ない」が残る。

## 方針

- Welcome の projection を全件返す accessor を足し、Open はそちらを読む。projection に無い registry
  entry は 0 件 overview として必ず補い、**Open は登録済みを 1 件も落とさない**契約にする。
- 一覧に選択追従の窓を入れ、溢れた件数を一覧下に出す。窓は状態を持たない純粋な計算にする。

## 受入条件

- [x] recent projection の表示枠より多く登録されていても、Open 一覧に全件並ぶ。
- [x] projection に現れない registry entry も 0 件 overview で並ぶ。
- [x] 端末に収まらない一覧は scroll し、選択行が常に見える。フッタが溢れで消えない。
- [x] 溢れている件数が一覧の下に出る。
