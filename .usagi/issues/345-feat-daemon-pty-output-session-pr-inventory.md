---
number: 345
title: feat(daemon): PTY output から session PR inventory を検出・永続化する
status: done
priority: high
labels: [daemon, core, pr]
dependson: []
related: []
created_at: 2026-07-17T23:44:26.464272+00:00
updated_at: 2026-07-18T00:10:22.943237+00:00
---

## 目的

daemon が所有する PTY output journal から HTTP(S) URL を安全に検出し、GitHub 系の `/pull/<number>` だけを canonical PR URL として session identity ごとの durable PR inventory に記録する。TUI、IPC、`gh` refresh は本 issue に含めない。

## 背景

v2 の daemon `TerminalRegistry` は出力を bounded journal に append して attach/replay に使うが、PR 検出・session identity への durable projection はまだ持たない。旧 TUI の `PrLink` と PR popup はあるものの、daemon authoritative なデータ経路ではない。

## 範囲

- core domain に daemon-owned PR inventory の型・canonical identity・状態を追加する。
  - canonical identity は URL 全体でなく、GitHub host の `https://<host>/<owner>/<repo>/pull/<positive-number>` とする。path suffix、query、fragment、周囲の句読点を除去する。
  - `http` / `https` の候補からのみ抽出し、control character、credential を含む URL、非 GitHub host、0・overflow・不正 percent encoding を fail-closed で除外する。
  - `PrState` は `Open | Closed | Merged | Dismissed` を表せるようにし、pin/dismiss は user-owned metadata として自動検出・後続 refresh による復活／上書きを防ぐ。
- daemon runtime で、PTY reader が journal に commit した出力 bytes だけを増分解析する。chunk 境界・UTF-8 境界・URL の分断でも検出を落とさず、同じ出力を再送しても inventory revision を不要に増やさない。
- inventory を stable `SessionId` に紐付け、daemon restart をまたぐ atomic durable store に保存・復元する。terminal/worktree path や TUI selection を identity の代わりに使わない。
- session ごとの revision 付き snapshot を作るための usecase/port を用意する。snapshot の IPC wire 化、subscription、`gh` 実行、TUI 接続は後続 issue とする。
- 既存 bounded output replay の retention semantics、input fencing、PTY ownership を変えない。

## 依存関係

- なし。
- 後続 #346（refresh + IPC）と #347（TUI）は本 issue の durable inventory / snapshot vocabulary に依存する。

## 受け入れ条件

- journal に commit 済みの output から、split chunk を含め `https://github.com/o/r/pull/42` を検出し、session ID に対して canonical URL を 1 件だけ永続化する。
- `/pull/42/files?x=1#y`、同一 URL の再出現、複数 terminal からの同一 PR は 1 canonical inventory entry に収束する。
- 非 GitHub URL、`http(s)` 以外、malformed URL、credential 付き URL、`pull/0`、整数 overflow は inventory を変更しない。
- inventory の restart round-trip、atomic write failure、duplicate replay、session identity 分離をテストする。
- pinned/dismissed entry を同じ URL の再検出が復活・上書きしない。Closed を serialize/deserialize できる。
- domain は IO/PTY/process に依存せず、daemon の出力処理は journal commit より前の bytes を観測しない。

## テスト観点

- pure canonicalizer/extractor: URL 境界、句読点、suffix/query/fragment、host allowlist、Unicode/invalid bytes、chunk 境界。
- domain reducer: dedupe、revision no-op、Closed、pin/dismiss precedence。
- infrastructure: durable store migration/round-trip/atomic failure。
- daemon integration: PTY output append → inventory projection、restart/replay/idempotency。

## 非目標

- `gh pr view` による title/state enrichment、retry/backoff、IPC subscription は #346。
- sidebar/modal/toast/browser 起動は #347。
- durable output journal を使った daemon crash 後の PTY continuation は対象外。
