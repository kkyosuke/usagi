---
number: 605
title: fix(daemon): worktree Git command から inherited repository 環境を除去する
status: done
priority: high
labels: [review, v2, daemon, git, security, correctness]
dependson: []
related: [470, 543, 584]
created_at: 2026-07-31T06:00:00+00:00
updated_at: 2026-08-01T00:03:37.285599+00:00
---

## Finding（P1 security/correctness）

`crates/daemon/src/infrastructure/session_worktree.rs::SystemGit::run` は `git -C <trusted repo>` を実行するが、daemon が継承した `GIT_DIR`、`GIT_WORK_TREE`、`GIT_COMMON_DIR`、`GIT_INDEX_FILE`、object/config override などを除去しない。Git は `-C` より repository environment を優先できるため、hostile environment で session create/remove/commit が別 repository を操作する。

## 最小修正方針

Git subprocess builder を一箇所に集約し、repository 選択・index・object database・config 注入に関わる `GIT_*` を allowlist 方式で消去する。必要な locale/terminal 環境だけを明示継承する。

## テストと受け入れ条件

- repo A を `-C` target、repo B を `GIT_DIR/GIT_WORK_TREE` にした 2-repo fixture で操作が A のみに反映される。
- `GIT_CONFIG_COUNT` / `GIT_CONFIG_KEY_*` 等の config injection も無効化される。
- create、nested worktree、remove の全 SystemGit 呼出しが同じ sanitized builder を通る。
