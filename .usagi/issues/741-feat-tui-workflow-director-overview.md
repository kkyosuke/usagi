---
number: 741
title: feat(tui): Director drawer に Workflows ビューを追加する
status: todo
priority: medium
labels: [v2, tui, workflow, director]
dependson: []
related: [740]
created_at: 2026-09-12T00:00:00+00:00
updated_at: 2026-09-12T00:00:00+00:00
---

## 問題

workflow は session に閉じた UI しか持たず、workspace 横断で「今いくつ動いていて、どれが人待ちか」を
見る手段が無い。Director drawer には Organization / Work Runs の route tree が既にあり、同じ場所に
置くのが自然である。

## 方針

- Director drawer に Workflows ビューを追加し、workspace 内の run を一覧する
  （session 名 / phase / 現在の担当 / 修正回数 / 経過時間 / PR）。
- 行を選ぶとその session の Workflow タブへ移動する。
- 判断待ちの run を先頭へ寄せる並び順にする。
- Work Runs と同じ route tree の規約（初回 open の着地点、Esc の戻り先）に従う。
- 一覧の取得は既存の workflow snapshot を session ごとに集約する形にし、新しい永続構造を足さない。

## 受入条件

- [ ] workspace 内の run が一覧表示され、判断待ちが先頭に来る。
- [ ] 行選択でその session の Workflow タブへ遷移する。
- [ ] 既存の Director route tree の open / Esc の規約を壊さない。
- [ ] run が 0 件のときの空表示がある。
- [ ] `document/03-tui.md` の Director 節に追記する。
