---
number: 284
title: feat(tui): Closeup Agent tab を daemon live runtime へ接続する
status: todo
priority: high
labels: [tui, agent, terminal, ipc, pty, integration]
dependson: [265, 279, 282, 283]
related: [263, 278]
parent: 227
created_at: 2026-07-13T12:00:00.000000+00:00
updated_at: 2026-07-13T12:00:00.000000+00:00
---

## 背景・根拠

`AgentLaunchAdapter`、`PaneRuntime`、Closeup controller の `Effect::LaunchAgent`、pending/live Agent tab reducer は個別に実装済みである。しかし実際に起動される `src/runtime/tui.rs` は legacy `WorkspaceView` の loop を駆動しており、これらを生成せず、Closeup の `agent` menu/command は pane placeholder を開くだけで daemon request・stream attach・input/resize に到達しない。

#265 は generic terminal の同じ live runtime bridge を所有する。Agent 専用の event pump、terminal client、tab state を複製せず、#265 が導入する daemon `TerminalPort`/renderer/stream loop に Agent launch adapter を合成する必要がある。#283 の fenced Agent completion/replay が利用可能になるまで pending Agent tab を live tab に確定してはならない。

## 目的

Closeup の menu と `agent [profile]` command を、daemon-authoritative `AgentLaunchAdapter` 経由で実行し、accepted/pending、success attach、safe failure、output/input/resize/exit/reconnect を同じ session-scoped live pane として end-to-end に動かす。TUI は local PTY/process を一切生成しない。

## スコープ

- #265 の runtime composition に Agent launch effect runner を追加し、`Effect::LaunchAgent` を stable workspace/session/profile/producer-issued operation ID の `DaemonRequest::Agent` へ一回だけ変換する。Closeup menu と parsed `agent codex` command は同一 effect path に正規化する。
- accepted を target-scoped pending Agent tab に投影する。#283 の same-operation fenced success completion だけを live Agent tab へ置換して selected tab を attach し、background completion は選択中の別 session/modal を奪わない。failure、unknown/stale/duplicate completion、disconnect は safe feedback と pending/state convergence に留める。
- #265 の shared `TerminalPort`、stream event pump、renderer を利用して Agent PTY stdout を live pane に描画し、選択中 Agent tab の non-prefix input と resize を daemon IPC へ一度だけ送る。exit は stream/pane state/tab selection を reducer contract に従って収束させる。
- reconnect 時は session-scoped saved full `TerminalRef` を daemon inventory で検証し、selected live Agent tab のみ attach/resume/resync する。name/path lookup、implicit replacement launch、local fallback はしない。
- daemon unavailable、missing executable/not authenticated、scope/profile rejection は product private detail を含まない inline feedback として描画する。pending tab が無限に残らない failure/replay policy を明示する。
- fake daemon client/terminal stream と injected PTY fixture による TUI integration/E2E を追加し、Closeup menu、`agent codex`、pending、output、input、resize、exit、detach/reattach、safe error を実行ループまで確認する。manual verification steps を `document/03-tui.md` の実装済み操作として更新する。

## 対象外

- daemon Agent runtime、profile adapter、CLI authentication probe、IPC completion schema（#283）。
- generic terminal runtime の実装を Agent 専用に複製すること、session lifecycle、raw argv/model/secret の UI 入力。
- terminal copy/search、daemon crash を越えた PTY FD continuation。

## 受け入れ条件

| 操作 | 観測する結果 |
| --- | --- |
| Closeup menu の Agent / `agent codex` | どちらも同一 `Effect::LaunchAgent` と one-shot IPC request になる |
| accepted → success | pending tab が表示され、matching operation の complete `TerminalRef` だけが live Agent tab となり選択中なら attach される |
| PTY stream | stdout が frame に描画され、selected Agent tab の input/resize が daemon に一度だけ中継される |
| exit / failure | tab、selection、feedback が reducer contract に収束し、別 session の tabs/modal を変更しない |
| disconnect/reconnect | process を止めず、inventory-validated selected tab が replay/resync され、replacement launch しない |
| unavailable/readiness error | safe actionable feedback を表示し、local spawn、credential/argv/raw error の露出を行わない |

## テスト方針

- application: menu/command effect parity、operation/profile forwarding、pending/final/failure/stale completion、session-scoped tab isolation。
- runtime integration: fake daemon client + fake terminal stream で attach/reconnect/input/resize/exit を実 event pump/renderer boundaryまで通す。
- PTY regression: #283 の fixture Agent を使い、実 PTY stdout と daemon IPC を通した Closeup 操作を確認する。実 Codex/Claude の install/credential は前提にしない。
