---
number: 281
title: fix(tui): session-scoped pane tabs and daemon Agent launch
status: done
priority: high
labels: []
dependson: []
related: [279]
created_at: 2026-07-13T11:17:27.225559+00:00
updated_at: 2026-07-14T12:34:12.675837+00:00
---

## 背景

実行時 TUI の legacy Workspace reducer が pane state を workspace 全体で 1 つだけ共有し、pane target を常に root にしている。そのため選択 session が変わっても tab が共有される。また Agent 操作は pending tab を追加するだけで、runtime から daemon Agent IPC を dispatch していない。

## 完了条件

- pane tab state と操作対象を選択中 session ごとに保持し、session 間で tab を共有しない。
- Agent tab / `agent` 操作が stable session identity を用いて daemon Agent IPC に送られ、daemon が PTY を spawn し、成功時に tab が live へ遷移する。
- reducer・IPC/spawn 経路を結ぶ回帰テストを追加する。
- 現行の TUI / daemon 仕様ドキュメントを実装に合わせて更新する。
