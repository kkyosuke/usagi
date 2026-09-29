---
number: 776
title: "feat(daemon): 動いている daemon の build を status と TUI の Daemon modal に表示する"
status: in-progress
priority: medium
labels: [v2, daemon, tui, cli, observability]
dependson: []
related: []
created_at: 2026-09-30T00:00:00+00:00
updated_at: 2026-09-30T00:00:00+00:00
---

## 問題

`usagi daemon` / `usagi daemon status` の出力 `usagi v4.8.8: daemon already running (pid 60991)` の `v4.8.8` は、
コマンドを実行した **client binary の version**（`AppInfo::describe`）であり、動いている daemon の version ではない。
更新直後、rolling update 前の旧 daemon が動いていても新しい version が表示されるため、operator は
「daemon も更新済み」と誤読する。

daemon は mandatory handshake の `ServerHello.build`（`BuildIdentity`）で自分の build を毎回 client に伝えており、
rollover 判定にも使っているが、どの surface もこれを表示しない。

| surface | client version | daemon version |
|---|---|---|
| `usagi daemon status` / `start`（already running） | 表示（prefix） | なし |
| TUI Daemon modal | なし | なし |

## 方針

事実の置き場所は daemon 自身の `BuildIdentity` 1 か所とし、`daemon.json` には version を複製しない。

1. **CLI**: `status`（既存の endpoint probe）と `start` の already running（1 回だけの hello）で観測した daemon build を
   行末に付ける。client build と異なれば、その旨を明示する。
   - `usagi v4.8.8: daemon running (pid 60991); daemon build v4.8.5 (abc1234) differs from this client v4.8.8 (def5678)`
   - 観測できない（refusal・未 probe）ときは従来の行のまま。
2. **TUI**: `DaemonMetrics` に daemon の `build` を追加（serde default で旧 daemon 互換）し、Daemon modal の Status に
   `daemon v… (commit)` を表示する。composition root が渡す client build と異なれば warning 色で client build も並べる。

## 完了条件

- status / start の出力に daemon build が出て、client と異なる場合を区別できる。
- TUI Daemon modal に daemon build が出て、client と異なる場合に warning 表示になる。
- 旧 daemon（metrics に build が無い）でも表示が壊れない。
- 対応 document を更新する。
