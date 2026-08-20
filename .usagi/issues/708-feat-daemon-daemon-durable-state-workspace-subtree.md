---
number: 708
title: feat(daemon): daemon の durable state を workspace 単位の subtree へ分割する
status: todo
priority: high
labels: [v2, daemon, lifecycle, workspace]
dependson: []
related: []
created_at: 2026-08-20T23:33:23.460781+00:00
updated_at: 2026-08-20T23:33:23.460781+00:00
---

設計は [document/proposals/17-multi-workspace-daemon.md](../../document/proposals/17-multi-workspace-daemon.md) が正本。本 issue はその段階 1。

## 問題・根拠

daemon の durable state が data directory 単位に置かれているため、data directory ごとに active daemon は 1 つ、
serve する workspace も 1 つに固定される。別 workspace を開くと typed workspace refusal になり、
`usagi daemon stop` してから目的の workspace で起動し直すしかない（live Agent を持つ daemon の stop は `--force` が要る）。

制約の出どころは fence ではなく state の置き場所である（proposal の「制約がどこから来るか」C1〜C4）。

- `<data-dir>/daemon/daemon.lock`（単一インスタンス lock）
- `<data-dir>/daemon/current.json`（locator）
- `<data-dir>/daemon/sessions.json`（`repository_root` を 1 つだけ持つ）
- generation registry / allocator / shard / dispatch / inbox / PR inventory

## 方針

`<data-dir>/daemon/w/<workspace-digest>/` の subtree へ上記を移し、置き場所の決定を単一の resolver
`daemon_state_dir(data_dir, workspace_root)` に集約する。data directory 自体は分けない（`workspaces.json`・
`settings.json`・`logs/` は共有のまま）。

- digest は bootstrap broker key と同じ作り（domain separation tag + length prefix + SHA-256、先頭 6 byte を hex 12 文字）。
- subtree には `root.json` を置き、canonical workspace root が一致しない場合は `-1`, `-2` … と probing する（別 workspace の state を書かない）。
- socket は `w/<digest>/g/<generation>/sock`（`generations/` を `g/` へ縮めて長さを吸収）。bind 前に `sun_path` 上限（macOS 104 / Linux 108）を検査し、超過時は `$USAGI_HOME` を短くする復帰手順を含む typed error で拒否する。
- legacy layout（`<data-dir>/daemon/*`）からの一方向 migration を daemon の起動時に行う。locator・record・socket は移さず破棄し、durable node だけ rename する。記録は `runtime-migration.json` と同じ形。

この段階では単一 workspace の挙動を変えない（layout の移動と migration だけ）。

## 受入条件

- daemon 側・client 側のすべての daemon state path が resolver を通る（`data_dir.join("daemon")` の直書きが product code に残らない）。
- digest の決定性・domain separation、`root.json` 不一致時の probing、socket path 長さ予算の境界、migration の着地先が unit / integration test で固定される。
- legacy layout を持つ fixture から起動すると、`sessions.json` の `repository_root` の subtree へ移行し、legacy 位置に durable node が残らない。
- カバレッジ 100% を維持する。
