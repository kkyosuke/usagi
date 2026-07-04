---
number: 107
title: feat(agent): guard-workspace に root モードを追加し root 行での Edit/Write・変更系 git を拒否する
status: in-progress
priority: high
labels: [orchestration, agent]
dependson: []
related: []
parent: 105
created_at: 2026-07-04T21:45:48.099614+00:00
updated_at: 2026-07-04T23:06:00.603153+00:00
---

## 背景

#106 の MCP ガードは issue/memory tool 経由の repo 変更を止めるが、root で動くコーディネータが `Edit` / `Write` で直接 `<repo>/src` や `document/` を書いたり、`Bash` で `git commit` したりする経路は塞げない。既存の worktree 閉じ込め（`usagi guard-workspace` を `PreToolUse` に差し込み、対象パスが cwd の worktree の外なら拒否。[04-orchestration.md#worktree への閉じ込め（メインリポジトリ保護）](../../document/04-orchestration.md#worktree-への閉じ込めメインリポジトリ保護)）は、**root 行では cwd == workspace root** のため「外」判定が働かず、`<root>/src` への書き込みを素通しする。

root 行では「repo を一切変更しない」ため、この閉じ込めを**より強い root モード**に切り替える。

## やること

- root で起動した Agent（cwd がワークスペースルート＝ `.usagi/sessions/<name>/` 配下でない）に差し込む `guard-workspace` を **root モード**にする。判定基準は既存の pre-commit 免除と同じ「cwd が `.usagi/sessions/` 配下かどうか」を流用する。
- root モードでは次を拒否する（`PreToolUse` で `permissionDecision: "deny"`）:
  - `Edit` / `Write` / `NotebookEdit` など**ファイル書き込み系ツール**すべて（パスに依らず）。
  - `Bash` のうち**リポジトリを変更する git サブコマンド**（`commit` / `add` / `push` / `merge` / `rebase` / `checkout -b` / `worktree add` など）。判定は堅牢な allow/deny 方針を決める（例: 変更系 git のみ deny し、読み取り系 `status` / `log` / `diff` は許可）。
- session worktree 内の Agent は従来の閉じ込め（worktree 外だけ拒否）を維持し、回帰させない。
- ロジックはテスト可能に分離し（payload → 判定を純粋関数化）、root モード/ session モードの分岐をユニットテストで網羅する。

## 受け入れ条件

- root 行の Agent が `Edit` / `Write` で任意パスを書こうとすると拒否される。
- root 行の Agent が `git commit` 等の変更系 git を実行しようとすると拒否される（`git status` 等の読み取りは通る）。
- session worktree の Agent は従来どおり worktree 内の編集ができ、worktree 外だけ拒否される。
- ドキュメント（[04-orchestration.md](../../document/04-orchestration.md) の閉じ込め節）に root モードを追記する。
