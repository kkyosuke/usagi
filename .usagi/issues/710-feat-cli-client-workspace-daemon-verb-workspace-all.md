---
number: 710
title: feat(cli): client の workspace 解決と daemon verb の --workspace / --all を追加する
status: todo
priority: high
labels: [v2, cli, daemon, workspace]
dependson: [708, 709]
related: []
created_at: 2026-08-20T23:33:56.462699+00:00
updated_at: 2026-08-20T23:33:56.462699+00:00
---

設計は [document/proposals/17-multi-workspace-daemon.md](../../document/proposals/17-multi-workspace-daemon.md) が正本。本 issue はその段階 3。#708 / #709 が前提。

## 問題・根拠

locator が workspace 単位になると、client は接続前に「どの workspace の daemon か」を決める必要がある。
また `usagi daemon status` / `stop` / `restart` は「その data directory の唯一の daemon」を暗黙の対象にしているため、
複数 daemon が動くと対象が曖昧になる。

## 方針

申告（`selected` / `bound`）の決定順は変えず、申告から subtree を引く規則を足す。

| 優先 | 申告 | subtree |
|---|---|---|
| 1 | `selected`（TUI が開いた workspace） | その canonical root の digest |
| 2 | `bound`（`USAGI_WORKSPACE_ROOT`） | 同上 |
| 3 | `bound`（cwd） | `w/*/root.json` を読み、cwd の祖先で**最長一致**する root の subtree。無ければ cwd を root として cold start |

session worktree（`<root>/.usagi/sessions/<name>`）自身が `.usagi/` を持つため、「最も近い `.usagi` を持つ祖先」では
誤判定する。判定材料は `root.json` に記録した canonical root だけにする（git は実行しない）。

CLI:

- `usagi daemon status` は既定で cwd の workspace、`--workspace <path>` で指定、`--all` で `root.json` から全 daemon を列挙する。
- `stop` / `restart` / `replace` も同じ選択規則。`--all` は `stop` にだけ用意し、live runtime を持つ daemon は現在どおり `--force` を要求する。
- `install-service` は変更なし（supervisor は既に workspace を pin する）。

## 受入条件

- session worktree・subdirectory・workspace 外の cwd から、それぞれ期待どおりの subtree（または cold start）に解決することを test で固定する。
- `daemon status --all` が 2 つの daemon を列挙し、`stop --workspace <path>` が指定した 1 つだけを止める結合テストが通る。
- 引数の文法・usage error・終了 status は `document/02-architecture.md` の process argv contract に従う。
- カバレッジ 100% を維持する。
