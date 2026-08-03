---
number: 640
title: fix(tui): 指示モードの Esc を root Agent へ渡し New chord と右ペイン dim を整える
status: done
priority: high
labels: [tui, bug, input, agent, director]
dependson: []
related: [576, 578, 580, 612]
created_at: 2026-08-03T21:53:17.219168+00:00
updated_at: 2026-08-03T21:53:22.511246+00:00
---

## 背景

指示モード（Director mode）の drawer で 3 点の入力・表示が実際の操作と噛み合っていない。

- **`Esc` が usagi に吸われる**。drawer が開いている間の `Esc` は drawer の close に予約されているため、選択中の
  root Agent CLI へ届かない。agent CLI は `Esc` を自身の中断・取消として読むため、drawer 内では中断操作が
  一切使えない。
- **New が plain `n` にしかない**。drawer で New を出すのに leader の後 plain `n` を押す必要があり、
  制御 chord（`Ctrl-O Ctrl-N`）は conversation の NextTab に取られている。
- **右ペインの dim が mode だけで決まる**。Switch のときだけ dim にして Closeup では明るく戻すため、pane が
  入力を所有していない frame（pending / interrupted tab 選択中、overlay 前面、drawer open）でも明るく描かれ、
  操作できる面と見分けられない。

## 対象

- drawer conversation の `Esc` を、live root Agent が attach している間はその PTY へ渡す（`0x1b` 1 回）。drawer の
  close は `Ctrl-O Ctrl-G` と header button が所有し、`Esc` が close になるのは `Esc` を受け取れる live
  conversation が無い frame（conversation 空、または pending / interrupted だけ選択中）に限る。picker が開いて
  いる間の `Esc` は従来どおり picker だけを閉じ、PTY へは届かない。
- drawer が開いている間だけ New と NextTab の chord を入れ替える（`Ctrl-O Ctrl-N` = New、`Ctrl-O n` =
  conversation の NextTab）。drawer が閉じている間の意味は変えない。入れ替えは frame loop が key を 1 度だけ
  retarget して行い、classifier は drawer の状態を持たない。
- 右ペインの dim を「その pane が入力を所有していないか」で決める。明度を戻すのは Closeup で選択中の tab が
  live terminal であり、前面に overlay / action modal / drawer が無い frame だけとする。
- `document/03-tui.md`（右ペイン dim、prefix 表、指示モードの入力所有権と context 表、interrupted tab の巡回）を
  実装に合わせる。

## 受入条件

- [x] live root Agent が選択されている drawer の `Esc` は PTY へ `0x1b` を 1 回送り、drawer は開いたままである。
- [x] live conversation が無い drawer の `Esc` は従来どおり drawer を閉じ、元の route / pane selection / focus へ戻る。
- [x] `Ctrl-O Ctrl-G` は live Agent が attach していても drawer を閉じる。
- [x] drawer が開いている間の `Ctrl-O Ctrl-N` は New picker を開き、`Ctrl-O n` は conversation を巡回する。
- [x] drawer が閉じている間の `Ctrl-O Ctrl-N`（NextTab）と `Ctrl-O n`（New）の意味は変わらない。
- [x] 右ペインは Switch、pending / interrupted tab 選択中、overlay 前面、drawer open のいずれでも dim で、
      live tab が入力を所有する frame だけ明度が戻る。
- [x] 実 PTY E2E を含む回帰テストと `document/03-tui.md` が実装と一致する。
