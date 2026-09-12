---
number: 739
title: feat(tui): workflow の判断待ちと PR 完了を通知する
status: todo
priority: high
labels: [v2, tui, daemon, workflow, notification]
dependson: [736]
related: [738]
created_at: 2026-09-12T00:00:00+00:00
updated_at: 2026-09-12T00:00:00+00:00
---

## 問題

`Phase::Waiting`（判断待ち）と `Phase::Ready`（PR 準備完了）は、まさに人を呼ぶべき瞬間である。
しかし現状はどちらも desktop notification も `user_decision` も出さず、Workflow タブを開いている人
だけが気づける。usagi には両方の配管が既にある。

## 方針

- `Waiting` への遷移で `user_decision` を起票する。本文に理由を入れ、選択肢として
  「催促する」「修正回数の上限を増やす」「中止する」（#745 の Stop に依存する選択肢は実装後に追加）を出す。
- `Ready` への遷移で desktop notification を出し、PR URL を添える。
- 同じ遷移で通知を重複させない（phase ごとに 1 回、`(run, phase)` の identity で抑止する）。
- 通知は daemon lane（#736）の観測から発火させ、TUI が開いていなくても届くようにする。
- 通知の有効・無効は既存の設定と同じ粒度で切れるようにする。

## 受入条件

- [ ] `Waiting` 遷移で理由付きの user decision が 1 件だけ作られる。
- [ ] `Ready` 遷移で PR URL 付きの通知が 1 回だけ出る。
- [ ] TUI を開いていない状態でも発火する。
- [ ] 同じ phase に留まっている間は再通知しない。
- [ ] `document/03-tui.md` に通知の条件と抑止規則を追記する。
