---
number: 390
title: fix(tui): drag 選択の反転表示が空白 padding / 空行で消える（agent 画面で選択が見えない）
status: done
priority: medium
labels: [tui, bug]
dependson: []
related: []
created_at: 2026-07-20T01:46:04.190605+00:00
updated_at: 2026-07-20T01:46:24.669641+00:00
---

## 概要

live terminal（特に agent tab）で mouse drag によるテキスト選択は内部的に機能している（copy は正しいテキストを OS clipboard に載せる）が、**選択範囲が画面にほとんど表示されない**という報告。agent が描く画面は空白 padding と空行が大半を占めるため、体感として「選択が可視化されない」。

## 原因

`crates/tui/src/usecase/application/terminal_screen.rs` の `render_row_selected` が、各行を**最後の非空白グリフ（`rposition`）まで**でしか描画しない。そのため:

- 行末の空白 padding を選択しても、その桁は描画対象外となり reverse-video が付かない（選択が非表示）。
- 複数行 block 選択の途中にある**完全な空行**は `""` に畳まれ、その行に選択の反転が出ない（選択に切れ目が入る）。

copy は選択開始時に snapshot した cells（padding を含む）から生成されるため成功する。これが「内部は動くが表示されない」の正体。

描画経路（`terminal_rows` → `display_rows_with_scrollback_selection` → `rows_with_scrollback_and_cursor_selection` → `render_row_selected`）と frame diff（style 変化を repaint する）自体は正しく、非表示は上記 trim が単一の根本原因。

## やること

- `render_row_selected` の描画幅を、選択がある行では選択終端（`usize::MAX` は grid 幅にクランプ）まで広げ、選択された空白セル・空行を reverse-video の空白として描く。
- 非選択行は従来どおり行末空白を trim する（回帰させない）。
- 選択が content 内で終わる場合は padding に反転を伸ばさない。
- CJK / wide glyph の continuation、cursor marker の優先、scroll / resize / frame diff、複数 tab、drag outside viewport、選択解除を回帰させない。

## 完了条件

- drag 選択の反転が選択した桁全体（行末 padding・範囲内の空行を含む）に表示される。
- 既存の copy / scroll / tab close / CJK / cursor 表示が回帰しない。
- pure render 回帰テスト（padding / 空行の反転、content 内終端で padding を反転しない）と projection 経路の回帰テストを追加する。
- `document/03-tui.md` の terminal 選択記述を更新する。
