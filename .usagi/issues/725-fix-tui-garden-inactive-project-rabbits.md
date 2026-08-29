---
number: 725
title: fix(tui): Garden で inactive project のうさぎも描く
status: done
priority: medium
labels: [v2, tui, garden]
dependson: []
related: [674, 687, 701]
created_at: 2026-08-29T00:00:00+09:00
updated_at: 2026-08-29T00:00:00+09:00
---

## 目的

複数 project を開いた状態で session garden を開くと、**active な workspace の session にしかうさぎが出ない**。
inactive project の区画は `project inactive` の空区画のままで、そこで動いている Agent が庭から見えない。
Garden の目的は「session 数と実行状態を一覧表より速く把握する」ことなので、開いている project の半分が
常に空区画では目的を果たさない。

## 現状

`WorkspaceDeck` は inactive project の session を read-only cache（`CachedGardenSession`）として持つが、
Agent membership は観測していないため `agents_observed: false` / `agents: []` で projection する。
widget 側は `agents_observed == false` の区画を `inactive_plot`（うさぎ無し）で描く。

daemon は多 tenant で、`DaemonRequest::AgentInventory { workspace }` は connection の bound tenant ではなく
**request が名指しした `WorkspaceId`** を daemon 全体の Agent record から filter して答える。したがって
他 project の inventory は read-only で観測でき、IPC protocol の追加は要らない。

## やること

- deck の slot に観測した Agent 群を持たせ、`garden_projection` が inactive plot へも配る。
- Garden が開いている間だけ動く bounded な observation lane を足し、開いている project 分の
  `AgentInventory` を専用 port で観測する。daemon の cold start はしない（read-only 観測）。
- lifecycle は依然 cache なので、`Available` 以外の cached lifecycle は今までどおり
  `cached · creating` などの静止表示にとどめ、うさぎは描かない。
- inactive project の session membership 自体（他 project で新しく生まれた session の出現）は cache のままで、
  別 issue として扱う。
