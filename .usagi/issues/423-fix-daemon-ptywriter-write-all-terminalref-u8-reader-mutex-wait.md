---
number: 423
title: fix(daemon): PtyWriter の二段ステートフル・プロトコルを write_all(&TerminalRef, &[u8]) へ改め、reader スレッドの Mutex 保持 wait() を解消する
status: todo
priority: medium
labels: [fix, daemon, review]
dependson: []
related: []
created_at: 2026-07-20T11:57:56.121691+00:00
updated_at: 2026-07-20T11:57:56.121691+00:00
---

## 背景

v2 全体の 7 サブシステム並列コードレビュー（2026-07-20）由来。file:line は 2f4dc5b6 時点で検証済み。

## 根拠（検証済み）

- `crates/daemon/src/usecase/terminal.rs:106-127` — `trait PtyWriter` は `select_terminal`（:110）→ `write_all`（:126）の**二段ステートフル・プロトコル**。「select と write の間に他スレッドが割り込まない」ことは、runtime 全体を包む 1 つの Mutex による直列化に暗黙依存している。
- 合成ルート `src/runtime/daemon.rs`: reader スレッドが `Arc<Mutex<PtyTerminal>>` を保持したまま `wait()` を呼ぶ — AgentPty :521-526（`exit_pty.lock().map_or(Err(()), |pty| pty.wait()...)`）、DaemonPty :632-637。`terminals: BTreeMap<String, Arc<Mutex<PtyTerminal>>>` は :449 と :570。

## 問題

- 二段プロトコルは呼び出し規約が暗黙で、直列化前提が崩れた瞬間に別端末への書き込み混線が起きる設計。
- reader が Mutex を握ったまま `wait()` するため、**EOF 後も exit しない子プロセス**（fd を継承した孫プロセス等）があると、その端末の Mutex が永久に解放されず、同じ面の入力・resize が停止し得る。

## 改善案（要検討）

- `PtyWriter` を `write_all(&TerminalRef, &[u8])` のワンショット API に変更し、選択状態を持たない設計にする。
- 外側の巨大 Mutex に依存せず、端末ごとに child（wait 用）と writer を別 Mutex に分ける。
- 関連: AgentPty/DaemonPty 重複統合 issue、巨大 Mutex 内の同期プロセス IO 分割 issue。

## 受け入れ条件

- [ ] PtyWriter がステートレスな API になり、select/write 間の割り込みという故障モードが型レベルで消える。
- [ ] EOF 後に exit しない子プロセスがあっても他端末の書き込み・resize が停止しないことがテスト（fake PTY）で固定されている。
- [ ] coverage 100% を維持する。
