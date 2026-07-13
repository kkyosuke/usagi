---
number: 295
title: fix(tui): Closeup Agent effect を daemon live runtime へ接続する
status: done
priority: high
labels: [tui, agent, terminal, ipc, pty, integration, regression]
dependson: []
related: [284, 923, 934, 938, 941, 942, 943]
created_at: 2026-07-13T12:25:16.562041+00:00
updated_at: 2026-07-13T23:04:04.102544+00:00
---

## 背景・根拠

#938 で daemon-owned Agent operation と Codex profile、#941 で accepted Agent tab の pending policy、#923 で daemon の IPC/実 PTY runtime が main に入っている。`crates/tui/src/usecase/application/agent_launch.rs` の `AgentLaunchAdapter` も `Effect::LaunchAgent` を `DaemonRequest::Agent` へ変換できる。

しかし実際に起動される `src/runtime/tui.rs` は legacy `presentation::run_with_settings` / `run_workspace_with_session_port` を直接呼び、Agent/terminal launch adapter、`PaneRuntime`、daemon terminal event pump を composition していない。そのため Closeup の Agent menu と `agent codex` は effect を発行しても IPC/PTY に届かず、既存 #284 の完了条件を満たしていない。#284 を再オープンして依存を巻き戻す代わりに、main の既存 API を前提にした corrective integration として本 issue を扱う。

## 目的

実 TUI effect loop に daemon-authoritative Agent launch/runtime bridge を接続し、Closeup の既定 `agent` と profile 指定 `agent codex` を安全に実 PTY pane まで到達させる。TUI は local PTY・process・Claude への implicit fallback を作らない。

## スコープ

- 現在の presentation loop を stateful runtime host へ接続し、workspace/session ごとの `PaneRuntime` と `AgentLaunchAdapter` を lifecycle に沿って保持する。既存 terminal launch adapter と client/stream implementation を共通化または合成し、Agent 専用の PTY / wire protocol を複製しない。
- `Effect::LaunchAgent` を controller 発行の workspace ID、session ID、operation ID、optional profile を保ったまま一回だけ daemon IPC Agent request に送る。Closeup menu と `agent codex` は同じ effect path を通す。profile 未指定は daemon の既定 Codex profile に委ね、Codex の失敗を Claude に置換しない。
- accepted は session-scoped pending Agent tab として描画し、同一 operation の fenced success/replay が返す完全な `TerminalRef` にだけ live Agent tab を attach する。失敗、mismatch、stale/duplicate completion は panic・focus 奪取・無限 pending を起こさず、対象 session の安全な feedback に収束させる。
- 既存 `PaneRuntime` 経由で Agent terminal の attach/resume/resync/output を描画し、選択中 live tab の non-prefix key input と resize を daemon に一度だけ送る。exit/close は tab と selection を reducer contract に従って回復し、disconnect/reconnect は inventory で完全一致を検証して selected live tab だけを再 attach する。
- daemon unavailable、CLI 未導入、PATH/認証/readiness、profile/scope rejection、attach failure は raw argv・credential・内部 error を露出せず、Closeup または workspace feedback に利用者が再試行・復旧判断できる safe message を表示する。
- #938、#923、#934、#941、#942/#943 と未マージの terminal/session branches を確認してから実装する。main に無い有用な terminal bridge があれば cherry-pick ではなく現行 main へ最小の設計で取り込み、既存 state machine と責務を重複させない。
- 実装済みの behavior だけを `document/03-tui.md`（操作・feedback）および必要時 `document/02-architecture.md`（composition boundary）の正本へ更新する。未実装の将来仕様は issue だけに残す。

## 対象外

- daemon Agent runtime、Codex/Claude adapter の spawn semantics、CLI credential 設定、IPC completion schema の再設計。
- local process/PTY fallback、profile の暗黙置換、terminal copy/search、daemon crash を越える PTY FD continuation。
- generic terminal feature を Agent 向けに再実装すること。

## 受け入れ条件

| 場面 | 観測する結果 |
| --- | --- |
| Closeup Agent / `agent` | profile absent の Agent request が一回だけ daemon へ送られ、daemon default Codex が選択される |
| `agent codex` | `profile=codex` を保つ同じ Agent request path を通り、Claude fallback しない |
| accepted → started | session-scoped `Agent (starting)` が同 operation の final `TerminalRef` で live Agent tab となり、選択中なら attach する |
| output / input / resize | fake daemon stream の output が実 frame に描画され、selected Agent tab の input/resize が各一回 daemon port へ届く |
| exit / close / reconnect | tab/selection/focus が安全に収束し、inventory-validated tab だけ replay/resync され、replacement launch はしない |
| failure | unavailable、readiness/CLI/PATH/auth、daemon/attach failure が panic せず safe feedback を表示し、pending を残さない |

## テスト方針

- TUI runtime integration: injected fake daemon/terminal port と fake event source を使い、実 effect dispatcher まで `agent` / `agent codex`、IPC intent、pending→live、output、input、resize、exit、detach/reconnect、failure feedback を検証する。
- Wire regression: framed fake daemon に `DaemonRequest::Agent` を検査させ、operation/profile/scope を assertion する。default profile と explicit Codex を別ケースにし、Claude request が出ないことも確認する。
- 実 wiring: CLI login を要しない fixture profile / fake spawned terminal を daemon composition に注入できる境界で、TUI host → IPC → PTY observation → pane render の最短経路を追加する。実 Codex/Claude binary・認証は test prerequisite にしない。
