---
number: 660
title: fix(tui): roles.toml editor を cursor・selection・scroll 対応の lossless editor にする
status: todo
priority: medium
labels: [review, stability, tui, editor, role, ux]
dependson: [655]
related: [620]
parent: 654
created_at: 2026-08-05T01:20:37.873349+00:00
updated_at: 2026-08-05T01:20:37.873349+00:00
---

## 症状

Overviewの`roles [workspace|global]`はversioned `roles.toml` source editorを開くが、現行editorで可能なのは末尾への文字追加・改行と末尾1文字のBackspaceだけである。描画もsource末尾14行を固定表示する。

既存sourceの中ほどや先頭にvalidation errorがある場合、利用者は後続sourceをすべて削除しないと修正できない。長いcatalogでは現在の編集位置、selection、上側の内容も確認できない。

#620はlossless/atomic editorを受け入れ条件に含めて完了したが、production UIの編集能力はappend/popに留まっている。#655はCtrl-S/pasteを含むproduction input adapterの欠落を扱い、本issueはmultiline editor自体の状態・操作・viewportを扱う。

## 修正方針

- source全体とchar-boundary cursor、selection anchor、preferred column、vertical/horizontal viewportを持つ純粋なmultiline editor stateを追加する。
- insert/paste/backspace/delete、Left/Right/Up/Down、Home/End、page移動、selection拡張をsourceの任意位置で扱う。
- rendererはcursorを含むwindowへ追従し、行番号または上下の残件indicatorでsource位置を示す。
- save時はeditorのsource全体を既存validation/atomic writerへ渡し、TOML formatting/comment/orderをlosslessに保つ。
- scope切替時のunsaved sourceを黙って捨てない。確認、dirty refusal、scope別draft保持のいずれかを明示契約にする。

## 受け入れ条件

- sourceの先頭・中間・末尾へASCII/CJK/複数行pasteを挿入できる。
- char境界を壊さず移動・範囲選択・置換・Backspace/Deleteが動く。
- 14行を超えるsourceでcursorへviewportが追従し、上下/page移動で任意行を表示できる。
- validation error後もsource、cursor、selection、viewport、dirty stateを保持して修正・再保存できる。
- `Ctrl-S`は#655のproduction input経路からexactly once saveし、保存中の二重submitを拒否する。
- Tabによるscope切替で未保存編集をsilent lossしない。
- tiny terminal、long line、CJK width、empty/final newlineをgolden/pure testで固定する。
- production Home frameでinput cursor markerが正しい端末cellへ出る。

## docs

`document/03-tui.md` のRoles editor操作、scope切替、dirty/validation挙動を更新する。
