---
number: 394
title: fix(tui): session create/remove の意味色と Switch Ctrl-A を復旧する
status: done
priority: high
labels: [tui, session, bug, parity]
dependson: []
related: [287]
parent: 227
created_at: 2026-07-20T03:57:18.717441+00:00
updated_at: 2026-07-20T04:24:29.429293+00:00
---

## 背景

Home sidebar の inline session create は pending skeleton を表示するが、その loading に semantic `Accent` を一貫して使う回帰を固定していない。session remove checklist も、選択候補と破壊的な remove action を Danger として明示する必要がある。さらに Switch の `Ctrl-A` は controller/reducer 契約を満たしつつ runtime 入力経路で create form を開くことを回帰させない。

#287 は旧 runtime adapter 前提の未着手 issue である。本 issue は現行 controller/runtime 構成で同じ Ctrl-A 契約を実装・回帰テストする。

## 受け入れ条件

- pending session create skeleton の activity glyph と名前が `Role::Accent` で描画される。
- session remove checklist は削除候補の選択と、remove を実行する action/hint を `Role::Danger` で明確に描画し、未選択・cancel/keep の安全な表現は既存の意味色を保つ。
- Switch の Ctrl-A は inline create form を開き `Selection::NewSession` を選択する。create form 中、Closeup action、live pane の input ownership は変えない。
- palette Role と renderer/reducer/runtime 経路を対象に回帰テストを追加し、`document/03-tui.md` を実装済み契約へ更新する。
