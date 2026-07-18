---
number: 349
title: feat(daemon): 既存 v2 state から legacy session を明示復旧する
status: done
priority: high
labels: [daemon, cli, session, migration, recovery]
dependson: [348]
related: []
created_at: 2026-07-18T00:52:53.084948+00:00
updated_at: 2026-07-18T01:06:25.371791+00:00
---

## 背景・根拠

#348 / #1040 は shared `<data-dir>/daemon/sessions.json` が未作成の最初の起動だけ、検証済み `<repo>/.usagi/state.json` を available managed session として採用する。既存利用者には failed `test-1` のような partial v2 lifecycle state が既にあり、この初回条件を通らない。そのため legacy session が sidebar から消えた状態を daemon restart だけでは回復できない。

restart で legacy record を自動 read/merge すると、v2 lifecycle の authority、stable identity、失敗状態を黙って混合または上書きする。これは禁止する。

## 目的

既存 v2 state を不変に保つ通常起動とは別に、operator が legacy sessions を完全検証して既存 durable lifecycle state へ明示的に追加採用できる recovery 経路を提供する。採用済み record の stable `SessionId` / `WorktreeId` は `sessions.json` に永続化し、TUI sidebar は restart 後も legacy UI metadata を保って表示する。

## 操作契約

- 人間向け入口は `usagi session recover-legacy` とする。既定は **dry-run** で、candidate、既存 v2 record、検証結果、衝突理由、採用時に発行する ID 以外の安全な結果を表示する。永続化は `--apply` を明示したときだけ行う。
- 実処理は CLI の local fallback ではなく、operation ID を伴う daemon IPC `SessionAction::RecoverLegacy` とする。CLI は結果を表示するだけである。TUI の起動時・sidebar 操作、daemon restart、MCP の通常 session tool はこの action を暗黙に呼ばない。MCP へ公開する場合も `session_recover_legacy` という別 tool だけにし、dry-run default と明示 `apply: true` を必須にする。
- normal daemon open/restart は `sessions.json` があれば legacy `state.json` を読まない。recovery dry-run も状態を変更しない。

## 検証と atomic commit

- daemon は trusted repository root と existing `sessions.json` を lock 下で再読込みし、legacy `WorkspaceStateStore` を同じ root から読む。candidate 全件について name grammar / uniqueness、canonical expected path `<repo>/.usagi/sessions/<name>`、linked-worktree marker、`git worktree list --porcelain` の path / repository / `usagi/<name>` branch binding を完全検証する。unreadable / missing / malformed legacy state も拒否する。
- 既存 v2 と legacy の同名は lifecycle が available、creating、deleting、failed を問わず競合として reject する。legacy 内の重複、同一 path/branch の重複、v2 ID / name invariant の異常も reject する。部分候補の採用、名前・path からの推測、worktree create/remove、legacy metadata の書換えは行わない。
- dry-run の plan と `--apply` の検証は別時点なので、apply は全件を lock 下で再検証する。validation failure、revision change、conflict、Git porcelain failure、store read/write failure は fail-closed とし、既存 `sessions.json` を変更しない。
- 成功時だけ、既存 `WorkspaceLifecycleState` の workspace ID・revision / lifecycle / operation journal / failed record と既存 session ID / worktree ID をそのまま保持し、全 validated legacy candidate を fresh stable IDs の available record として加えた一つの新 snapshot を atomic rename で commit する。
- write 前の失敗は no-write、atomic write 後の状態は durable snapshot だけを正本とする。legacy state は read-only metadata store のままである。

## スコープ

- core: recovery request/result と lifecycle store の lock/CAS/atomic envelope を追加し、既存 lifecycle reducer の create/remove semantics を変更しない。
- daemon: validator と dry-run/apply、safe error/result projection を実装する。raw porcelain、filesystem absolute path、legacy notes の内容を IPC/MCP error に露出しない。
- CLI: `session recover-legacy [--apply]` の結果表示と non-zero failure exit を追加する。
- TUI: recovery を発火させない。reconnect/restart 後の snapshot projection が newly adopted available sessions を stable runtime IDs で sidebar に表示し、legacy display name / origin / started_from / notes / PR / last_active を保つことだけを保証する。
- MCP: recovery tool を公開するなら CLI と同じ explicit dry-run/apply contract・結果 schema・safe output に揃える。公開しない場合は tool registry / docs に implicit recovery がないことを明記する。

## 完了条件

- existing v2 failed-only state と複数の valid legacy sessions で、dry-run は no-write、`--apply` は一回の atomic state update によって legacy session を追加する。daemon restart と TUI sidebar 再接続後、採用 session は表示され stable IDs を維持し、legacy UI metadata を失わない。
- v2 existing record と legacy same-name（available / creating / deleting / failed の全 lifecycle）、legacy duplicate name/path/branch、欠損/不正 record、broken linked worktree、repository/path/branch mismatch、porcelain failure、concurrent revision change、store failure の各ケースで既存 v2 state は byte-equivalent に残り、worktree effect と partial adoption はない。
- apply の前後で既存 v2 IDs/records と legacy UI metadata が変わらず、採用済み session の stable IDs は daemon restart 後も復元される。
- CLI parser / IPC / daemon runtime / store の tests、daemon restart regression、TUI `FsWorkspaceLoader` sidebar regression、MCP を公開する場合は schema / explicit apply regression を追加する。

## ドキュメント

`document/05-daemon.md` をこの recovery contract の正本に更新し、CLI 使用例、dry-run / `--apply`、fail-closed conflict matrix、restart が自動 recovery しないこと、metadata / IDs の所有境界を記載する。`document/02-architecture.md` は daemon-owned recovery writer と legacy metadata read-join への参照だけを持つ。
