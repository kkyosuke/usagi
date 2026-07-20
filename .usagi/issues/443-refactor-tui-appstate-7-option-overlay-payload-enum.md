---
number: 443
title: refactor(tui): AppState の 7 独立 Option overlay を payload 持ち enum に統合する
status: todo
priority: medium
labels: [refactor, tui, review]
dependson: []
related: []
created_at: 2026-07-20T12:02:57.243589+00:00
updated_at: 2026-07-20T12:02:57.243589+00:00
---

## 背景

v2 全体の 7 サブシステム並列コードレビュー（2026-07-20）由来。file:line は 2f4dc5b6 時点で検証済み。

## 根拠（検証済み）

`crates/tui/src/usecase/application/controller.rs:648-657`（`struct AppState`）に、排他のはずの overlay が独立 Option で 7 つ並ぶ:

`overlay: Option<Overlay>`（:648）・`note_editor`（:649）・`environment_editor`（:650）・`decision_overlay`（:653）・`pr_overlay`（:654）・`preview_overlay`（:655）・`create_session: Option<CreateSessionForm>`（:656）＋対の `create_session_error: Option<Notice>`（:657）。

## 問題

「同時に 1 つだけ開く」という不変条件が型で表現されず、reducer は開閉のたびに相互クリアと防御アーム（「他が開いていたら無視」）を書く必要がある。組み合わせ状態（2 つ Some）がバグとして表現可能になっている。

## 改善案（要検討）

- payload を持つ単一 `enum ActiveOverlay { None, Note(NoteEditor), Environment(...), Decision(...), Pr(...), Preview(...), CreateSession(...), … }` に統合する。
- 相互クリア・防御アームを削除し、遷移は enum の置換 1 操作にする。
- 既存 `Overlay` enum との統合も検討する。

## 受け入れ条件

- [ ] overlay 状態が単一 enum になり、「複数同時 Some」が型で不可能になっている。
- [ ] 開閉・切替の挙動が回帰しない（既存 controller test 維持）。
- [ ] coverage 100% を維持する。
