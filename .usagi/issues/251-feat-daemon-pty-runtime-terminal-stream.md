---
number: 251
title: feat(daemon): PTY runtime と terminal stream を実装する
status: todo
priority: high
labels: [daemon, terminal, runtime]
dependson: [250]
related: [218, 219, 220, 248]
created_at: 2026-07-12T22:32:36.062498+00:00
updated_at: 2026-07-12T22:32:36.062498+00:00
---

## 目的

#250 の product-neutral な `AgentProfile` / validated `LaunchRequest` / immutable `LaunchPlan` を daemon の一回の Agent runtime 実行へ接続する。daemon が runtime reservation、PTY lifecycle、terminal stream、durable snapshot、replay、reclaim に必要な基盤を一貫して所有し、client disconnect や daemon restart で起動を二重化しない。

Claude/Codex 固有の renderer、hook、設定形式はこの issue に含めない。daemon は adapter 名を分岐せず、profile/plan port と PTY/process port を消費する。

## Architecture ownership

| 層 | 所有する責務 |
| --- | --- |
| `usagi-core` | #250 の型・validation・typed failure を消費するだけ。CLI 文法、PTY、secret を追加しない |
| `crates/daemon/src/usecase` | resolve-once 後の runtime/terminal reservation、operation と launch snapshot の対応、queue/autostart、replay/reclaim orchestration |
| `crates/daemon/src/infrastructure` | injected process/PTY、runtime/terminal durable record、output journal、process identity/reclaim probe |
| presentation / IPC | 既存 terminal/session command を runtime state・typed error に投影する。product 固有 wire を増やさない |

## 受け入れ条件

- validated request を一度だけ profile/plan resolver へ渡し、runtime reservation と immutable launch snapshot を external spawn 前に durable に記録する。
- `AgentRuntimeId`、`TerminalRef`、operation owner/attempt、profile ID、plan schema/revision を対応付ける。profile を restart 時に黙って再解決して別の意味へ変更しない。
- PTY spawn、raw output journal、snapshot/replay、exit、detach/re-attach は #218/#248 の terminal contract を維持する。disconnect は runtime/PTY を停止しない。
- spawn 後の crash、response loss、PID evidence 不足、orphan/identity unknown は replacement spawn を block し、typed reclaim/reconcile state に収束する。
- verified process exit/reclaim 後だけ reservation・concurrency slot を解放する。old generation の terminal は trusted registry/cursor で replay/reconcile し、local fallback をしない。
- unsupported profile、stale plan revision、ambiguous spawn/reclaim、terminal-generation mismatch は fail-closed な typed result にする。

## 非対象

- Claude/Codex の executable、argv/flag、model allowlist、config materialization、MCP/hook provision、secret injection。
- IPC protocol・terminal stream schema・phase token・terminal authorization の再設計。
- daemon crash 後に PTY master を復元する broker/SCM_RIGHTS の実装。

## テスト方針

- **pure**: reservation/release、operation attempt fence、plan revision mismatch、runtime/terminal identity、reclaim state transition、concurrency accounting を table-driven reducer test で検証する。
- **fake**: fake profile-plan resolver、fake durable store、fake clock、fake process/PTY で resolve-once → persist → reserve → spawn → journal → exit と crash point / ACK loss / ambiguous identity を検証する。
- **daemon integration**: injected PTY と実 daemon control path で detach/replay、restart 後の retained stream/reconcile、orphan の no-replacement、verified exit の slot release を検証する。Claude/Codex 実 CLI は起動しない。

## 必要な document 更新

実装済み契約を `document/02-architecture.md` に daemon runtime ownership と adapter port の配置として反映し、`document/proposals/04-daemon-api.md` の launch/runtime snapshot・reclaim/error contract を実装値へ畳み込む。実装前の選択肢・未採用 crash-continuation は `document/proposals/05-daemon-lifecycle.md` / `07-pty-crash-continuation.md` に残し、現在仕様へ未実装の振る舞いを書かない。
