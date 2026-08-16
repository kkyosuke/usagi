---
number: 671
title: backlog: origin/main 1ef8a5cd v2 全体コードレビュー
status: done
priority: high
labels: [review, v2, backlog, epic]
dependson: []
related: [672, 673, 675, 676, 677, 678, 679, 680, 686]
created_at: 2026-08-13T00:02:18.580332+00:00
updated_at: 2026-08-16T22:32:31.476521+00:00
---

## レビュー基点

- reviewed commit: `1ef8a5cd6deeb91623034ac49f1b45277b6e032e`
- reviewed at: 2026-08-16
- 対象: usagi v2 全体（Rust source 310 files / 約234,133 lines。tests・examples・scripts・CI・config を含む監査集合は 378 files / 約255,071 lines）
- 観点: 正しさ、resource bound、durability、process/PTY lifecycle、authority/fence、IPC/MCP schema、TUI reducer/input/rendering/worker、install/CI

## 確認領域

| 領域 | 主な確認内容 |
|---|---|
| core | domain/state machine、durable stores、env resolver、git、IPC codec、VT/checkpoint |
| daemon | lifecycle、generation authority、resource allocator、PTY/output、worker shutdown、supervisor、PR refresh |
| TUI | reducer、input ownership、live terminal、render/frame diff、background pumps、platform helpers |
| CLI / MCP | argv、tool registry/schema、caller credential、dispatch/supervisor/decision route |
| scripts / CI / install | test recommendation、coverage exclusion、required contexts、installer/update |

## Finding 対応表

| priority | issue | invariant |
|---|---:|---|
| high | #675 | product-owned Git が repository-local hook / fsmonitor 等の helper を実行しない |
| high | #673 | pending user decision 待機が client worker / shutdown / generation retirement を塞がない |
| high | #672 | `op read` の stdout / stderr retained memory を stream ごとの hard cap 内に保つ |
| high | #676 | dispatch registry / inbox を count・byte・age bound、pagination、ack/GC と backpressure で bounded にする |
| high | #677 | user decision の入力・履歴・pending admission を hard bound と retention で bounded にする |
| medium | #678 | supervisor store / scheduler history を query・snapshot・journal・runtime metadata 全体で bounded にする |
| medium | #679 | PR refresh の `gh` child を bounded output と process-group cleanup で完了・回収する |
| medium | #680 | system clipboard helper の wait を deadline / cleanup 付きにする |
| medium | #686 | narrow Garden と key / pointer / wheel / resize の foreground input ownership を一致させる |

## 確定した根拠

- `confined_git_command` は inherited `GIT_*` を除去する一方、repository-local config を読む。実 Git fixture で `git worktree add` が `post-checkout`、issue source discovery の `git ls-files` が `core.fsmonitor` helper を実行し、marker file を作成した。
- `wait_for_user_decision` は期限なし `Pending` を 25 ms ごとの store read で待ち、client disconnect / daemon shutdown / generation retirement を観測しない。accept loop の shutdown は全 client worker を join するため、この handler が残ると lifecycle completion を塞ぐ。
- `crates/core/src/infrastructure/env_resolver.rs` は binding 128、secret 32、並列 child 4、30 秒 deadline を持つ一方、stdout / stderr reader が `read_to_end` で byte 上限を持たない。
- `crates/core/src/infrastructure/store/dispatch.rs` は registry と inbox を全件 read-modify-atomic-rewrite し、retention/GC がない。production `agent_inbox` は `since` / `unread_only` のみで limit/cursor/ack がなく、`mark_inbox_read` は production caller がない。
- `crates/core/src/infrastructure/store/user_decision.rs` は terminal decision を削除せず、全 state を mutation ごとに置換する。decision field / option countにも domain hard bound がない。
- `crates/core/src/infrastructure/store/supervisor.rs::events` は page 指定前に journal 全件を読む。journalだけでなく snapshot の `applied_events`、scheduler の start/wake reservations、terminal run自体にもretentionがない。
- `src/runtime/daemon.rs::GhProcess` は 5 秒 timeout 後に parent を kill/reap するが stdout byte cap と process group ownershipを持たない。
- `src/runtime/clipboard.rs` は TUI render thread 上で clipboard child の stdin writeと `wait()` を無期限に行い、helperがhangすると入力・描画もhangする。
- 手動 Garden は controller で無条件に overlay を開く一方、presentation だけが 64×14 未満で Home へ fallback するため invisible overlay が残る。pointer / wheel / resize も key と同じ wake-up owner へ到達せず、documented foreground input contract と不整合である。

## 問題なし／既存対策を確認した事項

- daemon generation authority、global allocator、terminal/Agent retention、PTY output pipeline、critical worker shutdown は既存 issue の fence/bound/GC と回帰テストを確認した。
- TUI live terminal の bounded scrollback、viewport、restore/reconnect、background pump は既存修正とテストを確認した。
- CLI/MCP の typed argv、caller credential、tool registry validation、1 MiB frame/IPC bound を確認した。
- `coverage-off` registry lint、test recommendation map、required contexts、installer checksum/version verification の契約を確認した。
- sidebar の repository-local diff helper 実行疑いは production と同じ command で再現せず、finding にしない。

## 完了条件

- 最優先の `op read` output bound（#672）を同じ PR で実装し、issue を `done` にする。
- 残る finding を独立した追跡可能な子 issue として起票する。
- fmt/check/clippy、risk-based selected tests、workspace full test、Markdown link check、PR CI required contexts を確認する。
