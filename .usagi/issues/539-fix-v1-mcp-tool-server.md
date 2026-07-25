---
number: 539
title: fix(v1/mcp): 長い tool 呼び出しが同一 server の後続要求を止めないようにする
status: todo
priority: high
labels: [v1, mcp, concurrency, ux]
dependson: []
related: [538]
created_at: 2026-07-24T22:38:13.057302+00:00
updated_at: 2026-07-24T22:38:13.057302+00:00
---

## 問題・影響

出荷 v1 の MCP server（`v1/src/presentation/mcp/mod.rs::serve_capped`）は stdio を **1 行読む → 処理する → 返信を書く** の完全逐次ループである。したがって 1 件の長い tool 呼び出しが、同じ server プロセスに来た**後続の全要求**を待たせる。

実地の症状: coordinator の MCP で `session_remove` が 120 秒超かかり、その間 `issue_search` / `session_list`（store lock を取らない読み取り操作すら）が一切応答しなかった。

`538` で store lock の保持区間を短くすると**別プロセス**の MCP / TUI は救われるが、この逐次ループが残る限り「remove を投げた同じ MCP 接続では他の tool が使えない」状態は変わらない。読み取り専用の `issue_search` / `session_list` / `issue_get` が数分待たされるのは、エージェント運用では実質的な全停止に見える。

## 成立条件 / 再現フロー

1. `usagi mcp` に `session_remove`（数分かかる巨大 worktree）を投げる。
2. 同じ stdio 接続に `issue_search` を投げる。
3. remove の応答が返るまで `issue_search` の応答が返らない。JSON-RPC の id は独立しているのに、framing ループが直列なため待たされる。

## 対象責務と非対象

### 対象

- **request 単位の並行 dispatch**。`serve_capped` の読み取りループは逐次のまま、`dispatch_line` の実行を bounded worker（小さな固定サイズの thread pool、または上限付きの thread spawn）へ渡し、応答は書き込み用の mutex で 1 行ずつ直列に書く。JSON-RPC は id で応答を対応付けるため、応答が要求順と入れ替わってよい。
- **`McpService` の `Sync` 要求とその充足**。並行実行のため trait object に `Send + Sync` を要求し、各 service 実装（`usagi.rs` / `session.rs` / issue / memory / composite router）が満たすことを確認する。満たせない内部状態があればそれを明示し、必要なら interior mutability を整理する。
- **上限と背圧**。無制限に thread を作らない。上限に達したら要求を queue し、queue も上限に達したら JSON-RPC error で明示的に断る（黙って詰まらせない）。
- **順序が必要な要求の扱い**。`initialize` は client が応答を待つため実害はないが、notification と `initialize` の扱いを明示する。同一 store を触る書き込み系は既存の store lock で直列化されるため、並行 dispatch でデータ競合は増えない（lock の取得順序は `538` の `teardown lock → store lock` 規約に従う）。

### 非対象

- store lock の保持区間の短縮（`538`）。
- `remove` 自体の高速化（rename-to-trash）。
- MCP を async ランタイム（tokio 等）へ移行すること。v1 は同期実装で出荷しており、依存追加なしに thread ベースで解く。

## 受入条件

- [ ] 長い tool 呼び出しの実行中に、同じ stdio 接続へ送った別の tool 呼び出しが（その tool 自身の所要時間で）応答する。
- [ ] 応答が要求順と入れ替わっても、各応答が正しい JSON-RPC id を持つ。
- [ ] 応答行が混ざらない（1 応答 = 1 行、書き込みは直列化される）。
- [ ] 並行数と queue に上限があり、超過は明示的な error 応答になる。
- [ ] 既存の逐次前提のテスト（parse error / too-long line / notification / initialize）が通る。

## 必須回帰テスト

- in-memory stream で「遅い tool」と「速い tool」を同時に流し、速い方が先に応答することを検証する。
- 応答の id 対応が保たれることを検証する。
- 並行して大量に要求を投げ、上限超過が error 応答になり server が生き続けることを検証する。
- 出力行が interleave しないことを検証する。
- parse error / too-long line / notification / EOF の既存挙動を固定する。

## docs / 移行影響

MCP の同時実行モデル（並行上限・応答順序が保証されないこと）を開発 docs に記載する。wire protocol は変わらない。
