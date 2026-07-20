---
number: 424
title: perf(daemon): 面全体の Mutex 内で実行される同期プロセス IO（launch fork+exec / session git 実行）を「予約→spawn→コミット」に分割する
status: todo
priority: medium
labels: [perf, daemon, review]
dependson: []
related: []
created_at: 2026-07-20T11:58:02.664449+00:00
updated_at: 2026-07-20T11:58:02.664449+00:00
---

## 背景

v2 全体の 7 サブシステム並列コードレビュー（2026-07-20）由来。file:line は 2f4dc5b6 時点で検証済み。

## 根拠（検証済み）

- `crates/daemon/src/usecase/session_runtime.rs:341` — `create` 内で `build_session_tree(&self.git, &self.repo_root, &path, ...)`、`:452` — `remove` 内で `remove_session_tree(&self.git, &path, force)`。`build_session_tree`（:689）＋`mirror_directory`（:705-739）は git worktree add 等の実プロセス実行と fs 操作を複数回行う。
- これらの `&mut self` メソッドは面全体のロック `type SharedSessionRuntime = Arc<Mutex<SessionRuntime<SystemGit>>>`（`src/runtime/daemon.rs:690`）の中で実行される。agent launch の実 fork+exec も同様に面の Mutex 内。
- 予約状態は既に存在する: `LifecycleEvent::ReserveCreate` が git 実行前に適用される（session_runtime.rs:324）が、git はその後もロック内で走る。

## 問題

1 つの session create/remove（git 実行込みで数百 ms〜数秒）や agent 起動中、同じ面の**全 IPC・全 agent の出力配信が停止**する。並行セッション運用でのレイテンシスパイクの主因になる。

## 改善案（要検討）

- 「lock 内で予約（Reserved/ReserveCreate）→ lock 外で spawn/git 実行 → lock 内でコミット（成功/失敗の確定）」の 3 段に分割する。予約状態は既存の語彙を流用できる。
- 失敗時のロールバック（予約解放）を必ず lock 内コミットで行う。

## 受け入れ条件

- [ ] launch / create / remove の実プロセス IO が面の Mutex 外で実行される。
- [ ] 予約→spawn→コミットの競合（同名 create の並行等）がテストで固定されている。
- [ ] coverage 100% を維持する。
