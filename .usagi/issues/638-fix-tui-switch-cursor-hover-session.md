---
number: 638
title: fix(tui): Switch の右ペインを cursor（hover）の session に追従させる
status: done
priority: high
labels: [tui, ui, parity]
dependson: []
related: [308]
created_at: 2026-08-03T21:14:53.518678+00:00
updated_at: 2026-08-03T21:21:32.600533+00:00
---

## 目的

v2 の Home Switch mode で、右ペインが常に active（command target）の session を描いており、左 sidebar の cursor を動かしても内容が変わらない。v1 は「選択（Switch）は highlighted session を preview する」挙動を持っていた（`v1/src/presentation/tui/home/event/mod.rs` の `drives_surface` / `selected_dir` 経路）ため、v2 で parity を失っている。

Switch は左 sidebar が navigation を持つ mode であり、cursor の移動は「他の session を見る」操作である。右ペインが追従しないと、session を切り替えるのに一度 Closeup へ入って戻る必要があり、Switch の目的である「見比べて選ぶ」ができない。

なお Switch 中の右ペイン dim（#308）は実装済みであり、本 issue はその dim 表示のまま中身を cursor に追従させる。

## 受け入れ条件

- Switch 中の右ペインの見出し session 名・tab strip・agent phase 行・live terminal viewport が cursor 行の session に追従する。
- 右ペイン footer が Switch では `[Switch] preview pane`、Closeup では `[Closeup] active pane` を表示し、preview が command 対象ではないことを示す。
- cursor の移動は active（command target）と live PTY 入力の宛先を変えない。
- `+ new session` 行、および Director drawer が開いている間は active target を描く（session を指していない／drawer が前面の handoff を所有するため）。
- 一度も pane を開いていない session を hover した場合は、未起動 target と同じ空の pane を描く。
- 同時に daemon へ attach する foreground terminal は 1 つのまま（preview がその 1 つを移す）。
- Switch 中の右ペイン dim（#308）は維持する。

## 関連

#308 は Switch の右ペイン dim。本 issue はその表示対象を cursor に追従させる。
