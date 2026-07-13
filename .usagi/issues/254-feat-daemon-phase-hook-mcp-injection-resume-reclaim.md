---
number: 254
title: feat(daemon): phase hook・MCP injection・resume/reclaim を接続する
status: done
priority: high
labels: [daemon, orchestration, mcp, lifecycle]
dependson: [251, 252, 253]
related: [219, 248]
created_at: 2026-07-12T22:34:02.359082+00:00
updated_at: 2026-07-13T00:05:49.201076+00:00
---

## 目的

#252 の Codex adapter と #253 の Claude adapter を、#251 の daemon-owned runtime に共通 orchestration として接続する。daemon が phase reporting、MCP provision/injection、resume/reclaim を runtime lifecycle と durable operation に結び、adapter は product 固有 renderer/provisioner に閉じたままにする。

## Architecture ownership

| 層 | 所有する責務 |
| --- | --- |
| `usagi-core` | #250 の product-neutral capability/request/plan と既存 phase/lifecycle typed value。product hook payload、secret、CLI 文法を持たない |
| Claude/Codex adapters | product 固有 hook/MCP/resume の render/provision contract。daemon へは scoped、redacted、typed result のみ返す |
| `crates/daemon/src/usecase` | runtime token/sequence を検証する phase ingest、capability-gated MCP injection、resume/reclaim decision、operation attempt fence と session/runtime projection |
| `crates/daemon/src/infrastructure` | scoped config/file/env provisioning、secret delivery、process identity probe、durable journal/store。secret は log/snapshot に書かない |
| presentation / IPC | 既存 `AgentPhaseReport`・operation/reconcile と typed status を投影する。product 固有 payload を public wire に載せない |

### authorization / secret boundary

- profile capability は「製品が機能を表現できるか」であり、IPC handshake capability、runtime phase token、terminal/session authorization を代替しない。
- phase report は daemon spawn 時に生成した one-runtime token、`AgentRuntimeRef`、generation/session fence、単調 source sequence を全て検証する。token/hook raw payload を durable state・event・log に残さない。
- MCP injection は validated profile capability と workspace/session authorization の両方を要し、adapter scoped provisioner を通す。secret は最小権限の process/file/env delivery に限定し、plan/snapshot/replay/IPC へ逆流させない。
- resume/reclaim は saved immutable plan provenance、adapter revision、verified process identity を照合し、unknown/ambiguous は fail-closed で人の明示 action を要求する。

## 受け入れ条件

- Claude/Codex の両 adapter を同じ daemon orchestration port に登録でき、daemon が product 名による lifecycle/authorization 分岐を持たない。
- phase hook report は runtime token、runtime/session/generation scope、source sequence を検証し、duplicate/stale/foreign/exited runtime の report を reducer へ適用しない。
- capability が無い profile/request は hook/MCP/resume を開始せず typed rejection とする。MCP provision/injection failure は spawn/input/retry の外部 effect と fence される。
- resume は profile/request/plan provenance と adapter revision を照合し、compatible snapshot のみ再開する。reclaim は verified identity のみを対象にし、ambiguous spawn/orphan/secret loss を自動再実行しない。
- MCP/phase/resume に関わる credential、token、raw config/hook payload、rendered argv を durable journal、terminal output annotation、IPC response、observability log に露出しない。
- #219 の phase/control semantics と #251 の terminal/reclaim state を尊重し、daemon restart・response loss・late adapter completion でも runtime phase/slot/operation を二重更新しない。

## 非対象

- Claude/Codex の CLI flag、hook payload schema、MCP 設定ファイル構文、credential 取得機構の core 化。
- 新しい client-facing product 固有 IPC command、terminal observation を hook 専用 source にすること。
- PTY crash master-fd continuation broker、unknown ownership の自動 kill/replacement spawn。
- 新たな agent product adapter の追加。

## テスト方針

- **pure**: capability/authorization decision、phase token/sequence/generation fence、resume provenance/revision、reclaim transition、secret-redaction policy を table-driven reducer/validator test で検証する。
- **fake**: fake Claude/Codex adapter、fake provisioner/secret provider、fake journal/process identity/clock を用い、MCP injection failure、token replay、restart/ACK loss、late completion、ambiguous reclaim を検証する。
- **daemon integration**: fake process を用い、両 adapter で spawn → provision/inject → phase report → detach/restart → resume/reconcile → exit を daemon control path で検証する。実 product CLI、network、実 secret は使用しない。

## 必要な document 更新

実装済みの phase ingest・MCP injection・resume/reclaim の所有者と authorization/secret boundary を `document/02-architecture.md` と `document/proposals/04-daemon-api.md` の現在仕様へ反映する。phase token、secret 名、product hook/config payload は API 仕様に記録せず adapter 内の実装契約に閉じる。未実装の crash continuation や追加 product は proposal/issue に残す。
