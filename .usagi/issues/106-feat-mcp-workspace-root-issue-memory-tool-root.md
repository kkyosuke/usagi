---
number: 106
title: feat(mcp): workspace root で issue/memory の書き込み系 tool を拒否する（root ガードレール）
status: done
priority: high
labels: [orchestration, mcp]
dependson: []
related: [104]
parent: 105
created_at: 2026-07-04T21:45:25.474322+00:00
updated_at: 2026-07-05T00:05:01.669127+00:00
---

## 背景

原則 2「root は git 追跡下のリポジトリを変更しない」を**規約でなく技術的に**担保する第一防壁。`UsagiMcpServer` は既に issue/memory を `worktree`、session を `workspace_root` に routing しており（`src/presentation/mcp/usagi.rs`、テスト `issue_and_memory_operate_on_the_worktree_not_the_workspace_root`）、この seam をそのまま判定に使える。

root で `usagi mcp` を起動したときは `worktree == workspace_root`（両者一致）になる。この一致を「root で動いている」の判定に使い、repo を汚す書き込み系 tool を拒否する。

> 補足（実装時に判明）: **メモリストア（`.usagi/memory/`）は git 管理外**（`.usagi/.gitignore` の `/*` で除外、`!/issues/` のみ再包含）。したがって root で memory を書いても追跡ツリーは汚れない。汚すのは **git 追跡下の issue ストア（`.usagi/issues/`）を書き換える issue tool のみ**なので、拒否対象は issue の書き込み系に限定し、`memory_save` / `memory_delete` は root でも許可する。

## やること

- `UsagiMcpServer`（合成層）で、`worktree == workspace_root`（正規化して比較）のとき次の tool を拒否し、`isError: true` で「root では実行不可・session 内で行うこと」を案内するツールエラーを返す:
  - `issue_create` / `issue_update` / `issue_delete`
- 次は root でも許可する（オーケストレーションに必要な読み取り・整形・session 操作、および git 管理外の memory 書き込み）:
  - `issue_get` / `issue_search` / `issue_to_prompt`
  - `memory_get` / `memory_search` / `memory_save` / `memory_delete`（memory は git 管理外）
  - すべての `session_*` と `session_delegate_issue`
- 判定は合成層に閉じ込め（sub-server は無改変）、ユニットテストで root=拒否 / session=許可の両分岐を網羅する（既存テストのスタイルに合わせ、`worktree == workspace_root` と `worktree != workspace_root` の 2 サーバで検証）。
- session（`worktree != workspace_root`）では従来どおり全 tool が動くこと（回帰なし）を確認する。

## 受け入れ条件

- root（`worktree == workspace_root`）で `issue_create` / `issue_update` / `issue_delete` を呼ぶと拒否され、案内メッセージが返る。
- root でも `issue_search` / `issue_get` / `issue_to_prompt` / `memory_search` / `memory_get` / `memory_save` / `memory_delete` / `session_*` は成功する。
- session worktree では全 tool が従来どおり成功する。
- ドキュメント（[03-commands/03-mcp.md](../../document/03-commands/03-mcp.md)）に「root では書き込み系 issue tool が拒否される（memory は git 管理外のため許可）」を追記する。
