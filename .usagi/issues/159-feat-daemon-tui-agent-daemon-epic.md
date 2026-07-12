---
number: 159
title: feat(daemon): TUI 非依存の agent ライフサイクル（daemon 化 Epic）
status: done
priority: medium
labels: [daemon, orchestration, architecture]
dependson: []
related: []
created_at: 2026-07-09T23:32:10.873452+00:00
updated_at: 2026-07-12T23:24:50.646924+00:00
---

## 目的

TUI プロセスが単独で抱えていた agent / シェルの PTY 所有、セッション監視、委譲 prompt の自動起動を常駐プロセス `usagi daemon` へ移す。TUI は daemon が所有する端末に attach するクライアントとなり、TUI を閉じても daemon 側の端末を継続する。

## 完了

- daemon の制御プレーン、セッション監視、IPC/attach プロトコル、PTY 所有、端末画面ストリーミング、Keys/Resize、TUI attach クライアント化を段階的に実装した（#160〜#167）。
- orphan adopt、マルチクライアント、および通知調停を実装した（#168）。

現在の実装境界は [アーキテクチャ](../../document/02-architecture.md) と [TUI](../../document/03-tui.md) を参照する。
