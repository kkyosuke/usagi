---
number: 407
title: usagi mcp: production E2E harness を整備し docs（07-mcp/orchestration guide）を実挙動へ整合する
status: done
priority: medium
labels: [mcp, docs]
dependson: [401]
related: []
parent: 400
created_at: 2026-07-20T04:55:08.806180+00:00
updated_at: 2026-07-20T07:31:47.332326+00:00
---

親: #400。依存: #401。各系実装が寄りかかる共有の **production E2E harness** を整備し、docs のドリフトを解消する。harness は早期に着手し、各系 issue（#402–#406）はその上に自系の E2E を足す。

## E2E harness（共有基盤）

- 現行の MCP E2E は initialize / daemon autostart 程度に留まる（`tests/` 配下に `usagi mcp` の tool 毎 durable 固定が無い）。
- `usagi mcp` を**実プロセス**で起動し、stdio JSON-RPC を送受信するテストドライバを用意する。実 daemon（一時 data-dir・隔離 workspace）を autostart させ、`initialize`→`tools/call`→durable effect の検証（ファイル/ストア/agent 配送）→`tools/list` 契約（47 件・schema）を回せる共通ユーティリティにする。
- fixture agent/worker を注入して agent 系（dispatch→complete→inbox）を決定的に固定できるようにする（実 CLI 認証に依存しない）。

## docs 整合（記載＝実装済み）

- `document/07-mcp.md`: issue/memory を実装済み store 系と読ませる記述（`:54-56`）、session 系を daemon IPC で動作と読ませる表を、各系の最終挙動に合わせて是正。tool 面の一覧を現状（実装済み/未実装）と齟齬なく保つ。
- `crates/cli/src/mcp/guides/orchestration.md`（resource `usagi://guides/orchestration`）: `tools/list` に載る実在 tool 名だけを使い、dispatch→observe→complete のワークフローを**実際に接続済みの tool**で説明する。未接続 tool を手順から外す（agent 誤誘導の防止）。

## 完了条件

- [ ] 実プロセス `usagi mcp` の stdio→実 daemon→durable→応答 を通す E2E harness が `tests/` に入り、CI で回る。
- [ ] harness を使う代表 E2E（少なくとも store/session/agent の各 1 本）が緑。各系 issue はこの harness に自系ケースを追加する（親の受け入れ条件）。
- [ ] `07-mcp.md` と orchestration guide が実挙動に一致（Markdown link check 緑）。
- [ ] Rust 差分を含むため full test / coverage 100% を満たす。
