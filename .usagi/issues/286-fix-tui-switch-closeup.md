---
number: 286
title: fix(tui): Switch / Closeup のモード遷移と入力所有を同期する
status: done
priority: high
labels: [tui, closeup, input]
dependson: []
related: [278, 269]
created_at: 2026-07-13T11:55:11.641377+00:00
updated_at: 2026-07-13T11:57:32.160595+00:00
---

## 目的

live Workspace runtime で Switch / Closeup の遷移を一箇所に正規化し、`Ctrl-O a` による Closeup 起動、`Ctrl-O o` による Switch 復帰、session 移動後の Closeup target を一致させる。

## 調査根拠

#278 は Closeup 内の tab/modal 入力所有を導入したが、Switch は `Key::Live(_)` を全て無視する。このため `Ctrl-O a` を Switch から実行しても Closeup が開かない。さらに Closeup 中の `PreviousSession` は sidebar の選択だけを移動し、Closeup modal の target label / action state が前の session のまま残る。

## やること

- Switch / Closeup から利用する mode transition helper を `WorkspaceUi` に集約する。
- Switch で `OpenCloseupModal` を受けたら、選択 target の Closeup を開き action modal に focus を渡す。
- Switch 遷移時に forced Closeup state を消し、Closeup と action modal が残留しないようにする。
- Closeup 中の previous-session 操作で target に対応する Closeup state を再構成する。
- regression tests と `document/03-tui.md` を更新する。

## 受け入れ条件

- Switch → Closeup と Closeup → Switch が排他的で、前面 modal が残らない。
- Closeup を開いた直後に action modal が入力を受ける。
- session 移動後に Closeup の表示 target と action target が sidebar 選択と一致する。
- tab / pending tab の state を別 session へ持ち越さない。
- `cargo test -p usagi-tui` を含む品質 gate が通る。
