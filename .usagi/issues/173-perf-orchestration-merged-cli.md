---
number: 173
title: perf(orchestration): merged/終了済みセッションの自動回収でエージェント CLI プロセスのメモリを解放する
status: todo
priority: medium
labels: [perf, orchestration]
dependson: []
related: [159]
created_at: 2026-07-10T20:47:21.179449+00:00
updated_at: 2026-07-10T20:47:21.179449+00:00
---

## 背景（メモリ調査 2026-07-11 実測）

ホスト全体のメモリ消費の支配項は usagi 本体ではなく**エージェント CLI プロセス**である:

| プロセス | 個数 | RSS/個 | 小計 |
|---|---|---|---|
| claude | 5 | 430–555 MB | ≈ 2.4 GB |
| codex | 3 | 180–230 MB | ≈ 640 MB |
| usagi TUI | 1 | ≈ 29 MB | 29 MB |

claude 1 プロセスで usagi TUI の 15–19 倍。usagi 内部の最適化では桁が届かず、**「不要になったセッションのエージェントを終了・回収してプロセス数を減らす」運用面の策が絶対値で最も効く**。usagi はエージェント CLI 自体のメモリを直接は減らせないが、ライフサイクルは握っている。

## やること（段階案）

1. **可視化と手動一括回収**: merged 済み（`session_status.merged` / PR badge が merged）または agent phase が ended のまま放置されているセッションを TUI 上で見分けやすくし、「merged セッションのペインを一括クローズ」操作を追加する。エージェント CLI プロセスの終了 = 数百 MB/セッションの即時回収。
2. **自動回収（設定でオプトイン）**: `settings.json` に例えば `auto_reclaim_merged_sessions`（bool または猶予分数）を追加し、merged 検知から猶予経過後にそのセッションのペイン（= エージェント CLI・シェル）を自動終了する。
   - 誤爆防止: agent phase が running/waiting のもの・dirty worktree は対象外。回収前に通知を出す。
   - claude は `--continue`、codex も resume 相当を持つため、誤って閉じても会話は復帰可能（各 CLI の resume 前提を確認して文書化する）。
3. `session_remove` まで進める完全自動化は本 issue の範囲外（コーディネータ運用の判断領域）。あくまで「プロセスの回収」まで。

## トレードオフ・関連

- エージェント CLI のインメモリ状態（画面上の途中出力など）は失われる。resume 可能性の担保と猶予期間・通知で緩和する。
- #159（daemon 化 Epic）とは独立に効く: daemon 化後も「エージェントプロセスをいつ畳むか」の判断は同じ。daemon の監視（`daemon status`）が回収トリガーの実装先になりうる。

## 確認方法

- merged セッションを回収した際、対象エージェント CLI プロセスが終了し RSS がプロセス分減ること。
- running/waiting/dirty のセッションが自動回収されないこと。
- カバレッジ 100% 維持。
