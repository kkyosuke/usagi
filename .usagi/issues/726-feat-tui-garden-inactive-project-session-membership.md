---
number: 726
title: feat(tui): Garden の inactive project の session 一覧も観測する
status: todo
priority: low
labels: [v2, tui, garden]
dependson: [725]
related: [725]
created_at: 2026-08-29T00:00:00+09:00
updated_at: 2026-08-29T00:00:00+09:00
---

## 目的

#725 で inactive project の **Agent membership** は Garden 表示中に daemon から観測するようになったが、
**session 一覧と lifecycle は依然 cache**（その tab が最後に active だったときの daemon snapshot）である。
そのため、他 project で agent が委譲して新しく生まれた session は、その tab を開くまで庭の区画にならない。
delegation で session が増えるのは usagi の常用パターンなので、庭が「今の全体像」を出し切れていない。

## 難所

`AgentInventory { workspace }` が request の名指した workspace を daemon 全体の record から答えるのに対し、
`Session::List` は **connection が bound した tenant** の lifecycle runtime が答える。したがって他 project の
session 一覧を読むには、その root を `ClientWorkspace::Selected` で宣言した接続が要る。

- 観測 lane が daemon を cold start しない（bootstrap lock を握らない）ことは #725 と同じく守る。
- 未保持 root への handshake は tenant の adopt を伴うため、screen saver の観測が workspace の
  採用契機になってよいかを決める必要がある（idle retire 済みの project を庭が起こし直す形になる）。
- cached lifecycle をやめて live lifecycle にすると、`cached · creating` などの表示区分も見直しになる。

## 受け入れ条件

- 他 project で生まれた session が、その tab を開かずに Garden の区画として現れる。
- 観測できない project（daemon 不在・上限超過）は今までどおり cache の区画にとどまる。
- Garden が閉じている間は観測しない。daemon の cold start をしない。
