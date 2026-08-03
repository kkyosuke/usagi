---
number: 641
title: fix(agent): root sandbox に起動する agent CLI 自身の state root を許す
status: done
priority: high
labels: [agent, daemon, sandbox, codex]
dependson: []
related: [530, 594, 629, 630]
created_at: 2026-08-04T00:00:00+00:00
updated_at: 2026-08-04T00:00:00+00:00
---

## 症状

workspace root（director / dir）で Codex を起動すると、Codex が起動前に失敗する。

```
WARNING: proceeding, even though we could not create PATH aliases: Operation not permitted (os error 1)
Codex couldn't start because its local database appears to be damaged.
  Location: /Users/<user>/.codex/state_5.sqlite
  Cause: attempt to write a readonly database
```

DB は壊れていない。root coordinator を包む OS sandbox が `~/.codex` への書き込みを拒否しているだけである。

## 原因

`usagi-core` の `usecase::claude_sandbox` は root mode の writable root に `$HOME/.claude` を**固定で**足していた。
launcher は Claude 専用ではなく Codex / sakana.ai も同じ launcher で包むため、Codex は自分の state
（`~/.codex/state_5.sqlite`、PATH alias）を書けず起動できない。session mode は writable root が own worktree だけで、
state は daemon-issued environment（`CLAUDE_CONFIG_DIR` / `TMPDIR`）で worktree 内へ向くため影響がない。

## 修正方針

- state root を `exec する program` から決める。`claude` → `~/.claude`、`codex` → `~/.codex`、
  `codex-fugu` → `~/.codex-fugu`。値の正本は `domain::settings::DefaultModel::state_directory`（`command` と同じ場所）。
- grant は起動する CLI に追従させ、他 provider の state へ広げない。usagi が launch しない未知 program には
  state root を与えない（fail-closed）。
- daemon 側の policy 検証（protected workspace / linked worktree の Git common dir との重なり）も同じ program から
  state root を決める。

## 回帰テスト

- root の writable root が program ごとに `~/.claude` / `~/.codex` / `~/.codex-fugu` になり、未知 program では
  `$HOME` 由来の grant が 0 件になる（`usagi-core` unit）。
- daemon の policy 検証が、`~/.codex` の中にある workspace を Codex では拒否し、state が別の provider では通す
  （root runtime unit）。
- 出荷 binary の `usagi claude-sandbox --mode root --home <home> -- <bin>/codex` が自分の state だけを書けて、
  他 provider の state を書けない（`tests/claude_sandbox_e2e.rs`）。
