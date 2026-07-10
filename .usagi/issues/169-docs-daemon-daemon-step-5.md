---
number: 169
title: docs(daemon): daemon 化を正本ドキュメントへ畳み込む（Step 5）
status: todo
priority: low
labels: [daemon, docs]
dependson: [168]
related: []
parent: 159
created_at: 2026-07-10T13:36:24.033055+00:00
updated_at: 2026-07-10T13:36:24.033055+00:00
---

Epic #159 の Step 5。Step 3b-4 / Step 4 まで挙動が確定した後、提案 [document/proposals/02-daemon.md](../../document/proposals/02-daemon.md) の内容を正本ドキュメントへ畳み込み、proposal はリンクスタブ化する（[proposals 運用](README.md) の方針）。

## やること

- **[04-orchestration.md](../../document/04-orchestration.md)**: セッション/端末のライフサイクルを「daemon が PTY を所有し TUI は attach クライアント」に更新。ペイン復旧・queued-prompt autostart の記述を daemon 前提に書き換え。「TUI を閉じても走り続ける」を仕様として明記。
- **[02-architecture.md](../../document/02-architecture.md)**: `presentation/daemon`（新入口）、`infrastructure/daemon_ipc`・`daemon_store`・`daemon_sessions_store`、`usecase/daemon`・`daemon_ipc` を層・モジュール一覧へ追加。
- **[data/](../../document/data/README.md)**: `<data-dir>/daemon/`（`daemon.json` / `stop` / `sessions.json` / `sock` / `terminals.json`）の永続化フォーマットを追記。
- **[03-commands/](../../document/03-commands/README.md)**: `usagi daemon start/stop/status`（ユーザー可視にするなら）を追記。
- `document/proposals/02-daemon.md` を「畳み込み済み」スタブにし、正本へリンク（[01-root-orchestration.md](01-root-orchestration.md) と同じ扱い）。

## 規約

- 「記載＝実装済み」に従い、**実装が確定した挙動だけ**を書く。未実装機能は書かない。
- 本 issue は Step 3b-4（#167）・Step 4（#168）完了後に着手（それまで挙動が固まらない）。

## 依存

- 前提: #168（Step 4）。
