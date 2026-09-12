---
number: 742
title: feat(daemon): workflow 履歴に phase を動かさないメッセージも残す
status: todo
priority: medium
labels: [v2, daemon, tui, workflow]
dependson: []
related: []
created_at: 2026-09-12T00:00:00+00:00
updated_at: 2026-09-12T00:00:00+00:00
---

## 問題

`append_history` は `previous != (phase, review)` のとき、つまり **phase か review を動かした
メッセージのときだけ**呼ばれる。planner が返した計画も、実装中の通常の `agent_message` も履歴に残らない。

結果として、実装が続いている数十分のあいだ Workflow タブは完全に無反応に見える。タイムスタンプも無いため、
「いつから止まっているのか」も分からない。

## 方針

- workflow scope のメッセージをすべて履歴に記録する（actor・kind・時刻）。
- phase を動かしたエントリには印を付け、UI で区別できるようにする。
- 既存の上限（100 件・本文 512 文字）は維持し、超過分は古い順に落とす。
- 既存レコードとの互換のため、時刻・kind は `serde(default)` で欠損を許容する。

## 受入条件

- [ ] phase を動かさないメッセージも履歴に残る。
- [ ] 各エントリに時刻と actor と kind がある。
- [ ] phase を動かしたエントリが区別できる。
- [ ] 上限 100 件・本文 512 文字が維持される。
- [ ] 旧レコードを読み込んでも壊れない。
