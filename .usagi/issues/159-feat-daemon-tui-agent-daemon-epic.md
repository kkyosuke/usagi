---
number: 159
title: feat(daemon): TUI 非依存の agent ライフサイクル（daemon 化 Epic）
status: todo
priority: medium
labels: [daemon, orchestration, architecture]
dependson: []
related: []
created_at: 2026-07-09T23:32:10.873452+00:00
updated_at: 2026-07-10T00:17:22.855547+00:00
---

## 目的

TUI プロセスが単独で抱えている「agent / シェルの PTY 所有」「セッション監視」「委譲プロンプトの自動起動」を常駐プロセス `usagi daemon` へ移し、**TUI を閉じても agent が走り続ける**ようにする。TUI は daemon 所有の端末に attach するクライアントになる（tmux / abduco 型）。

設計は [document/proposals/02-daemon.md](../../document/proposals/02-daemon.md) が正本。

## 段階的移行計画（各段独立 PR・カバレッジ 100% 維持）

1. **daemon スケルトン / 制御プレーン**（#160・実装済み） — `usagi daemon start/stop/status/serve`、単一インスタンスロック、stop シグナル、ファイルベースのレコード。まだ PTY は持たない。
2. **監視の移設**（#161・集約エンジン実装済み） — session monitor（phase 由来の activity 集約）を daemon へ移し、`sessions.json` に保存・`daemon status` で可視化。通知発火と daemon→TUI push は後段（Step 3/4）。
3. **PTY 所有の移設（核心）** — TerminalPool を daemon 側へ移し TUI を attach クライアント化。IPC socket（attach/screen/keys）。ここで「閉じても走り続ける」が成立。
4. **孤児 adopt・マルチクライアント・通知調停**の仕上げ。
5. **ドキュメント畳み込み** — 挙動確定後、proposal を 04-orchestration.md / 02-architecture.md / data/ へ畳み込む。

## 進捗

- Step 1（#160）: 実装済み。
- Step 2（#161）: 集約エンジン実装済み（通知・push は後段）。
- 次: Step 3（PTY 所有の移設・IPC socket）。
