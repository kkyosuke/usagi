---
number: 752
title: perf(daemon): 変化の無い workflow reconcile で durable write を行わない
status: todo
priority: medium
labels: [v2, daemon, workflow, performance]
dependson: [736]
related: [737]
created_at: 2026-09-12T00:00:00+00:00
updated_at: 2026-09-12T00:00:00+00:00
---

## 問題

`workflow::snapshot`（reconcile）は `DispatchStore::update_workflow` で記録を書き戻す。`update_workflow` は
変更の有無にかかわらず必ず atomic write を行うため、**新しい peer message が 1 件も無くても** record が
書き直される。

常駐 lane（#736）が 10 秒ごとに全 active run を sweep するようになったことで、この write は
「run の数 × 6 回/分」の定常的な durable write になる。進行が無い夜間や、レビュー待ちで長時間止まっている
run でも同じコストがかかる。

## 方針

- reconcile で record が実際に変化した場合だけ書き戻す。比較は `update_workflow` の呼び出し側で行うか、
  変化なしを表現できる API（`update_workflow_if_changed` 相当）を追加する。
- 比較は serialize 済みバイト列の同値、または必要な field の比較で行い、意味のある変化を取りこぼさない。
- cursor だけが進んだ場合は「変化あり」として扱う（次回の replay 範囲が変わるため）。

## 受入条件

- [ ] 新しい peer message が無い reconcile で durable write が発生しない。
- [ ] 変化がある場合は従来どおり atomic に書き戻す。
- [ ] 書き込み回数を数える test がある。
