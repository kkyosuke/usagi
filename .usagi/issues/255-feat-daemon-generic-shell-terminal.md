---
number: 255
title: feat(daemon): generic shell terminal を安全に起動する
status: done
priority: high
labels: [daemon, terminal, runtime, security]
dependson: [251]
related: [218, 250]
created_at: 2026-07-12T23:01:20.256776+00:00
updated_at: 2026-07-12T23:07:53.046778+00:00
---

## 目的

Claude/Codex/AgentProfile を通さず、daemon が通常の interactive shell terminal を起動・所有する経路を実装する。client は terminal/shell profile の stable ID と表示上必要な scope・geometry だけを要求し、program、cwd、非 secret env は daemon が code-defined な安全な profile または trusted local settings から解決する。client から任意の raw command / argv / env を daemon に送って実行させない。

既存 #218 の `TerminalRef`、PTY、terminal stream と #251 の reservation、durable record、replay、reclaim を消費する。generic terminal は `AgentRuntimeId` を作らず、agent runtime の phase hook / MCP injection / adapter provisioning とは独立に保つ。

## Ownership

| 層 | 責務 |
| --- | --- |
| `usagi-core` | generic terminal profile ID、safe launch intent、typed validation / rejection の product-neutral な値語彙。shell command string・secret・PTY を持たない |
| `crates/daemon/src/usecase` | trusted profile/settings の resolve-once、terminal reservation と immutable snapshot、detach/replay/reclaim orchestration |
| `crates/daemon/src/infrastructure` | injected PTY/process、trusted local settings reader、durable terminal record/output journal、process identity probe |
| IPC / client | profile ID と terminal scope/geometry のみを typed request として投影する。raw command、argv、env、secret を wire/event/log に載せない |

## 受け入れ条件

- daemon は allowlist された terminal/shell profile または trusted local settings のみから `program`、`cwd`、非 secret env を一度だけ解決する。IPC client の raw command / argv / env は受理・転送・実行できない。
- launch 前に `TerminalRef`、operation owner/attempt、profile ID、plan schema/revision と non-secret provenance を durable に保存し、secret・raw shell command・rendered command string は durable state、IPC event、log に残さない。
- generic terminal は `AgentRuntimeId` / agent profile / phase token を生成せず、Agent runtime hook、MCP injection、agent adapter provisioning を呼ばない。
- #218 の PTY、raw terminal output、attach/detach、cursor replay の contract を維持する。client disconnect は PTY/process を停止しない。
- daemon restart、response loss、PID evidence 不足、orphan / identity unknown は replacement spawn を block し、#251 の verified exit / reclaim policy に従う。verified exit または reclaim 後だけ reservation を解放する。
- stale profile revision、unknown / disabled profile、scope/cwd validation failure、terminal generation mismatch、ambiguous reclaim は fail-closed な typed result になる。

## 非対象

- Claude/Codex/AgentProfile の executable、argv/flag、model、config materialization。
- agent runtime、phase hook、MCP injection、resume の実装または変更。
- client supplied raw command、任意 argv/env、remote command execution API。
- daemon crash 後に PTY master を復元する broker / SCM_RIGHTS の実装。

## テスト方針

- **pure**: profile allowlist、trusted settings validation、scope/cwd fence、non-secret snapshot/redaction、profile revision mismatch、reservation/release と typed rejection を table-driven test で検証する。
- **fake**: fake profile/settings resolver、durable store、clock、PTY/process/identity probe により resolve-once → persist → reserve → spawn → journal → detach/replay → exit と ACK loss / ambiguous identity / secret-redaction を検証する。
- **daemon integration**: injected PTY と実 daemon control path で generic terminal の detach/re-attach/replay、restart 後 reconcile、orphan の no-replacement、verified exit の release を検証する。Claude/Codex 実 CLI は起動しない。

## ドキュメント更新

実装済みの generic terminal launch ownership、trusted settings boundary、保存対象/非保存対象、Agent runtime との分離を `document/02-architecture.md` と `document/proposals/04-daemon-api.md` の現在仕様へ反映する。未実装の profile discovery、remote execution、crash continuation は proposal / issue に残し、現在仕様には書かない。

## 依存関係

- **#218**: `TerminalRef`、PTY、terminal stream の基盤を利用する。
- **#250**: agent launch contract の設計境界を参照するが、generic terminal は `AgentProfile` を消費しない。
- **#251**: daemon-owned runtime reservation、durable snapshot、replay、reclaim の実装済み契約を generic terminal の terminal-only path に適用する。本 issue は #251 に依存する。
