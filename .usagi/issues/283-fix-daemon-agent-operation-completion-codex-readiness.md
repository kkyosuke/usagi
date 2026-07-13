---
number: 283
title: fix(daemon): Agent operation completion と Codex readiness を root IPC runtime に接続する
status: todo
priority: high
labels: [daemon, agent, ipc, pty, codex, integration]
dependson: [271]
related: [252, 253, 263, 265, 270]
parent: 227
created_at: 2026-07-13T12:00:00.000000+00:00
updated_at: 2026-07-13T12:00:00.000000+00:00
---

## 背景・根拠

#271 は root daemon に `AgentRuntime`、`AgentPty`、`DaemonRequest::Agent` の admission を組み立てた。しかし IPC response は accepted と terminal hint の一回限りであり、operation の success/failure/replay を client が取得・subscribe する production path がない。`AgentLaunchAdapter` はこの final event を消費して初めて pending Agent tab を live terminal に置換できるため、現状の TUI は completion を受け取れない。

また root の default profile は `claude` の literal で、Codex を既定にする要求と一致しない。Codex/Claude adapter は injected provisioner の typed `ExecutableUnavailable` を表現できるが、root provisioner は executable と authentication readiness を検査しないため、実機では不透明な PTY/spawn failure に遅延する。認証値・argv・環境値は durable state、wire、UI feedback に出してはならない。

## 目的

daemon-owned Agent operation を accepted から fenced success/failure/replay まで IPC の正本として提供し、TUI が exact `TerminalRef` だけへ attach できるようにする。既定 profile を Codex とし、`agent codex` と omitted profile の双方を同じ registry で解決する。Codex/Claude の executable 不在または non-secret な readiness failure は spawn 前に安全で行動可能なエラーへ正規化する。

## スコープ

- Agent admission record に operation status、safe completion、scope/generation/execution fences を durable に保持し、same semantic `OperationId` の reconnect/replay が accepted/final を一意に返す IPC vocabulary を追加する。same ID の異なる intent は idempotency conflict のまま拒否する。
- terminal output/exit は既存 Agent owner の journal/terminal contract を通し、exit を operation final と terminal lifecycle へ一度だけ反映する。disconnect は subscription を外すだけで process/PTY/completion を停止しない。
- Agent final の success は complete fenced `TerminalRef`、failure は safe category と user-actionable message だけを返す。raw argv、credential、token、config path/content、OS error の詳細を wire/durable snapshot/log/UI feedback に露出しない。
- root composition の default profile を `codex` に変更し、explicit `codex` と `claude` は既存 `AdapterRegistry` で選ぶ。unknown profile は spawn 前に safe error とする。
- root Codex/Claude provisioner に injectable command/readiness probe を置く。binary discovery と product-owned non-secret authentication/readiness check を spawn 前に行い、unavailable/not-authenticated/materialization failure を typed safe failure に写像する。probe は test fake に差し替え可能とし、秘密情報を読み出し・記録しない。
- daemon root を起動する black-box Unix IPC + injected fixture executable/PTY test を追加し、launch → accepted → output → attach → input → detach → reattach → exit → final/replay を確認する。実 Codex/Claude binary/network/credential は必須にしない。missing executable と not-authenticated fake の safe failure、default Codex と explicit `agent codex` を同じ suite で確認する。
- 実装済みの operation/replay/readiness 契約を `document/04-ipc.md` と `document/05-daemon.md` に反映し、fixture だけで再現できる手動確認手順を正本またはテスト近傍の Markdown に記載する。

## 対象外

- Closeup renderer、keyboard loop、pane/tab registry の接続（#284）。
- session lifecycle/scope resolver、generic shell terminal launch の再設計。
- credential の自動ログイン、ブラウザ認証、secret 保存、CLI argv/model UI、daemon crash 後の PTY FD continuation。

## 受け入れ条件

| 操作 | 観測する結果 |
| --- | --- |
| omitted profile / explicit `codex` | どちらも Codex profile を解決し、一度だけ durable admission される |
| explicit `claude` / unknown profile | Claude は同じ registry path、unknown は spawn 前の safe failure となる |
| fixture launch と IPC reconnect | 同じ operation の accepted/final と fenced `TerminalRef` を replay し、二重 spawn しない |
| output/input/resize/exit | daemon PTY の stream と terminal actor を通り、exit は tab consumer が収束できる final/event を一度だけ出す |
| CLI 不在・未認証 fake | `install/sign in` を示す safe message だけを返し、raw command error・credential・argv を含めない |
| client disconnect | Agent process と journal は継続し、reattach は retained output を同一 terminal から再開する |

## テスト方針

- pure/fake: operation state machine、completion/replay/idempotency/fence、profile default/explicit selection、redaction、readiness category。
- daemon integration: fake registry/probe/store/PTY で output、input、exit、disconnect/reconnect、late/duplicate completion を検証する。
- root black-box: temporary data dir、fixture executable、Unix IPC client のみを使用する。実 CLI と認証を必要としない。
