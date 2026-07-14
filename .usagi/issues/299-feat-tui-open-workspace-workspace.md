---
number: 299
title: feat(tui): Open Workspace から安全に workspace 登録を解除する
status: done
priority: medium
labels: [feat, tui, workspace]
dependson: []
related: []
created_at: 2026-07-14T22:30:42.962666+00:00
updated_at: 2026-07-14T22:34:13.352556+00:00
---

## 目的

Open Workspace 画面で、選択中の workspace を安全に登録解除できるようにする。登録解除はグローバル registry だけを変更し、対象ディレクトリ・Git worktree・workspace 内データは削除しない。

## 変更内容

- Open Workspace の選択中 entry に登録解除の shortcut と確認状態を追加する。
- 確認中は `y` または Enter で実行し、`n` または Esc で取り消す。
- registry mutation は既存 core の `workspace::remove` を通す loader port に委譲し、成功時だけ Open list から実際に削除された path を反映する。
- footer から実態と合わない `Tab` / `C` の説明を除去・訂正し、登録解除操作を案内する。

## 受け入れ条件

- 選択中 workspace 以外を登録解除しない。空・filter miss 状態では何も実行しない。
- 明示確認なしでは registry を変更しない。cancel は一覧・registry を変更しない。
- 登録解除が削除するのは registry entry のみで、ディレクトリや workspace データを削除しない。
- 成功時に Open list と選択状態が安全に更新される。
- key hint は実装された Open 操作だけを示す。

## テスト

- Open state: request/cancel/confirmed removal と選択・filter/unite 状態の整合。
- presentation: 確認文と訂正済み footer。
- screen graph: loader に選択 path だけが渡ること、confirm/cancel の動作。
