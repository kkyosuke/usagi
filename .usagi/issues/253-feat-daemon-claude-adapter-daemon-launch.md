---
number: 253
title: feat(daemon): Claude adapter を daemon launch に接続する
status: done
priority: high
labels: [daemon, agent, claude]
dependson: [250, 251]
related: [142, 145, 146]
created_at: 2026-07-12T22:33:12.438314+00:00
updated_at: 2026-07-12T23:04:37.483270+00:00
---

## 目的

#250 の共通 contract と #251 の daemon runtime port を使い、Claude 固有の launch plan renderer と provisioner を adapter 境界に閉じて daemon launch へ接続する。Claude の CLI 文法、hook/config 形式、model の解釈、secret materialization は core と共通 daemon orchestration に漏らさない。

## Architecture ownership

| 層 | 所有する責務 |
| --- | --- |
| `usagi-core` | product-neutral profile/capability/request/plan validation の public contract。Claude 型や flag を持たない |
| Claude adapter module/crate | Claude profile の support 宣言、request → argv plan renderer、設定/MCP/hook materialization、secret を含まない provision result |
| `crates/daemon/src/usecase` | adapter port を profile ID で解決し、validated plan/provision result を #251 の reservation/PTY lifecycle に渡す |
| `crates/daemon/src/infrastructure` | provisioner が要求する scoped file/env と process/PTY を注入して実行し、secret を durable record/log に残さない |

Claude adapter の write-set は Claude 専用 module・fixture・test に限定する。Codex adapter (#252) の renderer/provisioner を参照せず、両 issue は #250/#251 後に並行できる。

## 受け入れ条件

- Claude profile の valid request を adapter が shell-neutral `program`/`argv` plan に render し、daemon が #251 の reservation を通じて起動できる。
- Claude の mode、model selector、initial/resume prompt、product capability の supported/unsupported を adapter が typed result で決める。CLI flag spelling や model allowlist を core に追加しない。
- config/MCP/hook の Claude 固有形式は adapter 内で materialize し、daemon に渡すのは scope 済み provision result と non-secret launch plan だけである。
- executable/config error、adapter revision mismatch、provision failure は spawn 前に typed failure とし、spawn 後の不明結果は #251 の reclaim policy へ委譲する。
- argv、raw hook payload、credential、secret path/content を operation/terminal snapshot、IPC event、error/debug log に保存しない。
- Codex adapter を参照せず、同じ adapter port を満たす独立実装として並行可能である。

## 非対象

- Codex 固有 adapter・共有 renderer の強制統合・product 間 capability の再定義。
- phase hook の daemon ingest、MCP injection の共通 orchestration、resume/reclaim の product 横断接続（#254）。
- core public contract、terminal stream/PTY lifecycle、IPC wire の変更。
- 本物の Claude CLI・実 credential を使う E2E。

## テスト方針

- **pure**: Claude request/capability matrix、argv rendering、unsupported option、revision/provenance、redaction を table-driven test で検証する。
- **fake**: fake provision filesystem/environment と fake process adapter で config/hook/MCP materialization の scope、failure cleanup、secret 非永続化、adapter-port contract を検証する。
- **daemon integration**: fake Claude adapter/process を daemon launch resolver に登録し、reservation → provision → PTY spawn → terminal exit の接続と typed failure を確認する。実 Claude executable/network/secret は使わない。

## 必要な document 更新

実装済みの adapter seam と Claude 固有設定が core/daemon durable state に入らない境界を `document/02-architecture.md` に反映する。利用者向けに実行可能となった設定だけを該当 v2 document へ追加し、CLI 文法・hook payload・secret 名は仕様 document の正本に複製しない。未実装 product 固有差分は proposal/issue に残す。
