---
number: 301
title: feat(tui): Open Workspace から安全に workspace 登録を解除する
status: done
priority: medium
labels: [feat, tui, workspace]
dependson: []
related: []
created_at: 2026-07-14T22:30:42.962666+00:00
updated_at: 2026-07-14T22:46:31.297718+00:00
---

## 目的

Open Workspace 画面で、選択中の workspace を安全に登録解除できるようにする。登録解除はグローバル registry だけを変更し、対象ディレクトリ・Git worktree・workspace 内データは削除しない。

## 変更内容

- Open Workspace の選択中 entry に Ctrl-D の登録解除 shortcut を追加する。
- workspace 終了時と同じ確認 modal を重ね、Enter または o で実行、Esc または c で取り消す。
- registry mutation は既存 core の workspace::remove を通す loader port に委譲し、成功時だけ Open list から実際に削除された path を反映する。
- footer から実態と合わない Tab / C の説明を除去・訂正し、Ctrl-D を案内する。

## 受け入れ条件

- 選択中 workspace 以外を登録解除しない。空・filter miss 状態では何も実行しない。
- 明示確認なしでは registry を変更しない。cancel は一覧・registry を変更しない。
- 登録解除が削除するのは registry entry のみで、ディレクトリや workspace データを削除しない。
- 成功時に Open list と選択状態が安全に更新される。
- key hint は実装された Open 操作だけを示す。

## テスト

- Open state: request/cancel/confirmed removal と選択・filter/unite 状態の整合。
- presentation: Ctrl-D 表示、確認 modal、訂正済み footer。
- screen graph: loader に選択 path だけが渡ること、confirm/cancel の動作。
