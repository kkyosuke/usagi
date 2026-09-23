---
number: 747
title: feat(workflow): 停止した reviewer を再割当できるようにする
status: todo
priority: medium
labels: [v2, daemon, workflow, agent]
dependson: [745]
related: []
created_at: 2026-09-12T00:00:00+00:00
updated_at: 2026-09-12T00:00:00+00:00
---

## 問題

reviewer は実装担当が最初に handoff した Agent に束縛される。その Agent が停止すると run は
`Waiting` になり、**同じ Agent を回復する以外の復旧手段が無い**。回復できない場合、run は
中止するしかない。

また実装担当が一度も handoff しない場合 reviewer は未割当のままで、`Recipient::Reviewer` 宛の指示は
`reviewer is not assigned yet` という内部エラー文字列がそのまま画面に出る。

## 方針

- 停止・回復不能な reviewer を、同じ session 内の別 Agent へ再割当できるようにする。
- 再割当は既存の身元検証（runtime profile + dispatch binding + session）を満たす相手にだけ許す。
- 再割当時、進行中の review request は無効化し、新しい request ID で再レビューを要求する
  （古い承認を引き継がない）。
- reviewer 未割当時のエラー文言を、利用者が次の行動を判断できる安全な文言に置き換える。

## 受入条件

- [ ] 停止した reviewer を同一 session の別 Agent へ再割当できる。
- [ ] 再割当は身元検証を満たす相手にだけ許可される。
- [ ] 再割当で進行中の review が無効化され、古い承認が引き継がれない。
- [ ] reviewer 未割当時の文言が内部エラー文字列でなくなる。
- [ ] `document/03-tui.md` を更新する。
