---
number: 751
title: feat(workflow): 開始設定をテンプレートとして保存する
status: todo
priority: medium
labels: [v2, tui, workflow]
dependson: [746]
related: [745]
created_at: 2026-09-12T00:00:00+00:00
updated_at: 2026-09-12T00:00:00+00:00
---

## 問題

担当の組合せ（planner / implementer / reviewer）は既に workspace 単位で保存されるが、goal の雛形と
修正回数の上限は毎回入力し直しになる。同種の作業を繰り返すとき、この入力が毎回の摩擦になる。

## 方針

- 開始設定（担当の組合せ・修正回数の上限・goal の雛形）を名前付きテンプレートとして workspace に保存する。
- 開始前フォームでテンプレートを選ぶと各欄に展開し、そのまま編集して開始できる。
- 既存の `remember_workflow_agents` による「最後に成功した組合せ」の保存はテンプレート未選択時の既定として残す。
- テンプレートは workspace 設定と同じ場所に保存し、新しい永続レイヤを増やさない。

## 受入条件

- [ ] 名前付きテンプレートを保存・選択・削除できる。
- [ ] 選択で担当・上限・goal 雛形が展開され、編集してから開始できる。
- [ ] 既存の「最後に成功した組合せ」の挙動が保たれる。
- [ ] `document/03-tui.md` を更新する。
