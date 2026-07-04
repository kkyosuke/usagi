---
number: 104
title: fix(tui): MCP で作成した新規セッションがサイドバーに即反映されないことがある
status: todo
priority: high
labels: [tui, orchestration]
dependson: []
related: [100]
created_at: 2026-07-04T13:25:09.114976+00:00
updated_at: 2026-07-04T21:24:10.626941+00:00
---

## 背景

root コーディネータが MCP の `session_create` / `session_delegate_issue` で新しいセッションを作っても、稼働中 TUI ホーム画面のサイドバー（セッション一覧）に反映されないことがある。自律オーケストレーション（委譲したセッションが即座に一覧へ現れて進捗を追える）の前提として直したい。

## 現状の実装（＝反映経路は既に存在する）

以下の連鎖で反映される設計になっている（新設ではなく既存経路の不具合として直す）:

- `session_create` / `session_delegate_issue`（`presentation/mcp/`）→ `usecase::session::create` → `WorkspaceStore::save` で `<workspace>/.usagi/state.json` を書き込む。
- TUI ホーム（`presentation/tui/home/`）は state.json の **mtime 監視スレッド**（コミット `90dec69` #536 で追加、`SESSIONS_WATCH_POLL` = 500ms ごとに stat）で変化を検知し、`usecase::workspace_state::recorded_sessions()` で再読込 → `apply_pending_refresh` → `refresh_sessions()`（一覧を丸ごと置換）する。
- キー入力が無くても idle tick（500ms）で loop が起きるため、最悪 ~1 秒で反映されるはず。

にもかかわらず反映されないケースがあるため、既存経路のどこが取りこぼしているかを特定して直す。

## 有力な原因（調査で特定した着眼点）

1. **mtime 検知の取りこぼし**（最有力）: 変化トリガが state.json の mtime のみ。ファイルシステムの mtime 解像度（秒単位）や、セッション作成の連続書き込みで前回値と mtime が同値になるケースだと変化を検知できない。mtime 一致でも内容差分を拾えるようにする（サイズ併用・ハッシュ・世代カウンタ等）か、作成直後に明示 refresh を叩く。
2. **統合（unite）モードの監視漏れ**: 監視対象が `workspaces[0]` の state.json のみ。2 つ目以降の workspace に作られたセッションは監視対象外で反映されない。全 workspace 分を監視する。

## やること

- 上記いずれが実際の原因かを再現・特定し、既存の反映経路（mtime 監視スレッド／`apply_pending_refresh`）を修正する。
- MCP で作成・委譲した新規セッションが、人手の操作なしにサイドバーへ反映されることを確認する。
- テストを追加（依存注入で mtime 同値・複数 workspace のケースを再現）。

## 受け入れ条件

- TUI 稼働中に MCP `session_delegate_issue` / `session_create` を呼ぶと、キー操作なしでサイドバーに新規セッション行が現れる（mtime 同値になるタイミングでも取りこぼさない）。
- unite モードで副 workspace に作られたセッションも反映される。
- ドキュメント（design/ のホーム画面、必要なら 04-orchestration）を更新する。
