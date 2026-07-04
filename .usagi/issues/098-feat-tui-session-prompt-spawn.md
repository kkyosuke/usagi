---
number: 98
title: feat(tui): session_prompt 起動時キューの自動起動 — キュー検知でエージェントペインをバックグラウンド spawn する
status: todo
priority: high
labels: [orchestration, tui]
dependson: []
related: []
created_at: 2026-07-04T05:09:01.647770+00:00
updated_at: 2026-07-04T05:09:01.647770+00:00
---

## 背景

`session_prompt`（および `session_delegate_issue`）が積む起動時キュー（`~/.usagi/agent-prompts/`）は、**人がそのセッションのエージェントペインをフレッシュ起動するまで消費されない**。コーディネータ役のエージェントが issue をセッションへ委譲しても、人がペインを開くまで子エージェントは走り出さず、「人間は issue と PR の管理だけ」という自律オーケストレーションの最重要ギャップになっている。

## やること

- 動作中の TUI（ホーム画面）の監視スレッドが `agent-prompts/` のキューを検知したら、対象セッションのエージェントペインを**バックグラウンドで自動 spawn** し、キュー済みプロンプトを最初のメッセージとして着手させる。
- spawn の仕組みは**ペインの復旧**（起動時にスナップショットからバックグラウンド spawn する既存機構）を流用できる見込み。attach しなくても agent phase フックにより左ペインのバッジ（`▶ running` / `✓ done`）は動く。
- 自動起動の ON/OFF を設定（例: `autostart_queued_prompts`）で制御する。既定値は要検討（意図しない稼働・トークン消費を避けるなら既定 OFF、自律運用を主眼にするなら既定 ON）。
- TUI が起動していない間にキューされたプロンプトの扱い（次回 TUI 起動時に自動 spawn するか）も決める。

## 受け入れ条件

- TUI 稼働中に MCP 経由で `session_delegate_issue` を呼ぶと、人がペインを開かなくても子エージェントが着手する。
- 設定で無効化すると従来どおり「次のフレッシュ起動時に消費」へ戻る。
- ドキュメント（03-mcp / 04-orchestration / 05-settings）を更新する。
