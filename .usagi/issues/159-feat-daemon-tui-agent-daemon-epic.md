---
number: 159
title: feat(daemon): TUI 非依存の agent ライフサイクル（daemon 化 Epic）
status: todo
priority: medium
labels: [daemon, orchestration, architecture]
dependson: []
related: []
created_at: 2026-07-09T23:32:10.873452+00:00
updated_at: 2026-07-10T12:32:34.032287+00:00
---

## 目的

TUI プロセスが単独で抱えている「agent / シェルの PTY 所有」「セッション監視」「委譲プロンプトの自動起動」を常駐プロセス `usagi daemon` へ移し、**TUI を閉じても agent が走り続ける**ようにする。TUI は daemon 所有の端末に attach するクライアントになる（tmux / abduco 型）。

設計は [document/proposals/02-daemon.md](../../document/proposals/02-daemon.md) が正本。

## 段階的移行計画（各段独立 PR・カバレッジ 100% 維持）

1. **daemon スケルトン / 制御プレーン**（#160・実装済み）
2. **監視の移設**（#161・実装済み）— session monitor を daemon へ、`daemon status` で可視化。
3. **PTY 所有の移設（核心）**
   - **3a. IPC + attach プロトコルの土台**（#163・実装済み）— Unix domain socket、`subscribe`/`list_sessions`、監視変化時の `Sessions` push。
   - **3b. PTY 所有の移設**
     - **3b-1. daemon が PTY を所有**（#164・実装済み）— IPC `spawn`/`kill`、daemon が端末を子として所有し、**クライアント切断後もプロセスが生存**（e2e 実証）。単一端末で「閉じても走り続ける」が成立。
     - **3b-2. Screen ストリーミング** — daemon 側 vt100 権威、購読者へ画面差分 push。
     - **3b-3. TUI を attach クライアント化** — `Keys`/`Resize`、`TerminalPool` 置換。
4. **孤児 adopt・マルチクライアント・通知調停**の仕上げ。
5. **ドキュメント畳み込み** — 挙動確定後、proposal を 04-orchestration.md / 02-architecture.md / data/ へ畳み込む。

## 進捗

- Step 1（#160）/ Step 2（#161）/ Step 3a（#163）/ Step 3b-1（#164）: 実装済み。
- 次: Step 3b-2（Screen ストリーミング）→ 3b-3（TUI クライアント化）。ここまで来ると TUI からも「閉じても走り続ける」端末が使える。
