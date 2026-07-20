---
number: 390
title: fix(tui): drag 選択が mouse release で消える／空白 padding・空行で反転しない（agent 画面で選択が見えない）
status: done
priority: medium
labels: [tui, bug]
dependson: []
related: []
created_at: 2026-07-20T01:46:04.190605+00:00
updated_at: 2026-07-20T02:28:02.722585+00:00
---

## 概要

live terminal（特に agent tab）で mouse drag によるテキスト選択が、**マウスを離すと画面から消える**という報告。drag 中は反転表示されているが、release した瞬間に反転が消える（copy 自体はできている）。加えて、agent 画面のように空白 padding と空行が大半の画面では、選択の反転が一部しか出ない副次バグもあった。

## 原因（2 点）

### 1. release で選択が破棄される（主因・報告された症状）

`crates/tui/src/presentation/mod.rs` の pointer `Up` handler が `controls.take_copy_text()` を呼び、`take_copy_text` は `self.selection.take()` で**選択 state を取り出して消す**。release 直後の描画では `controls.selection()` が `None` になり反転が出なくなる。copy は release 時に OS clipboard へ書けているため「内部は動くが表示が消える」状態だった。

### 2. 空白 padding / 空行で反転しない（副次）

`crates/tui/src/usecase/application/terminal_screen.rs` の `render_row_selected` が各行を最後の非空白グリフ（`rposition`）までしか描画しないため、選択された行末 padding と範囲内の空行に reverse-video が付かない。

## やること

- **選択を release 後も保持する**。`Up` では選択を消さず copy だけ行い、反転を画面に残す。新しい drag が始まったとき／focus が別 terminal に移ったときだけ選択を置き換え・解除する。
  - drag 進行中フラグ（`dragging`）を導入し、「進行中の drag を extend」か「finished 選択の上に新規 drag を begin」かを区別する（`has_selection` だけでは区別できない）。
  - 空選択（クリックのみ等）は残さず drop し safe feedback を出す。stray release で再 copy・clipboard 消去をしない。
- `render_row_selected` の描画幅を選択終端（`usize::MAX` は grid 幅にクランプ）まで広げ、選択された空白セル・空行を反転空白で描く。非選択行は従来どおり行末空白を trim。
- CJK / wide glyph の continuation、cursor marker 優先、scroll / resize / frame diff、複数 tab、drag outside viewport、選択解除を回帰させない。

## 完了条件

- drag を離しても選択の反転が画面に残り、次の drag / focus 変更まで表示され続ける。
- 反転が選択した桁全体（行末 padding・範囲内の空行を含む）に出る。
- copy / scroll / tab close / CJK / cursor 表示が回帰しない。
- pure render・controls lifecycle・projection 経路の回帰テストを追加する。
- `document/03-tui.md` の terminal 選択記述を更新する。
