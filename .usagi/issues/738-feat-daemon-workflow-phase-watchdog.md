---
number: 738
title: feat(daemon): workflow の phase に証拠の watchdog を付ける
status: todo
priority: high
labels: [v2, daemon, workflow, agent]
dependson: [736]
related: [739]
created_at: 2026-09-12T00:00:00+00:00
updated_at: 2026-09-12T00:00:00+00:00
---

## 問題

実装・レビューの進め方（planner を先に handoff、編集させない、reviewer は別 Agent、full SHA で
review_request、3 往復で停止）は `initial_prompt` の英文 1 本に載っている。daemon は**証拠を厳格に検証する**
が、モデルが契約から外れたときの失敗モードは**沈黙**である。

review_request が一度も来なくても run は `Implementing` のまま留まり、timeout も催促も無い。利用者からは
「動いているのか死んでいるのか分からない」状態が続く。

## 方針

- phase ごとに「最後に証拠（peer message / 配送確認 / 検証結果）を観測した時刻」を持つ。
- 一定時間証拠が無い run を `Waiting` に落とし、具体的な理由を出す
  （例: 「実装 Agent が review_request を送っていません」）。
- 自動催促は既定で行わず、利用者の操作（催促 instruction の送信）を 1 アクションで出せるようにする。
  自動催促を入れる場合も、同じ idempotent な instruction ID 規約に従う。
- 担当 Agent が live かどうかの既存判定（`reconcile_runtime` の suspended_phase）と二重に `Waiting` へ
  落とさない。理由は区別できる文言にする。
- 閾値は phase ごとに変えられるようにし、既定値をドキュメントに書く。

## 受入条件

- [ ] 証拠が一定時間無い run が理由付きで `Waiting` になる。
- [ ] 既存の「Agent 停止による Waiting」と理由が区別できる。
- [ ] 証拠が再び観測されたら自動で元の phase へ戻る。
- [ ] 時刻は注入した clock で駆動し、test が実時間に依存しない。
- [ ] `document/03-tui.md` に閾値と表示文言を追記する。
