---
number: 171
title: fix(daemon): 孤児 `usagi daemon serve` プロセスの残留を防ぐ（テスト teardown と data dir 消失時の self-shutdown）
status: todo
priority: high
labels: [perf, daemon, test]
dependson: []
related: [159, 164]
created_at: 2026-07-10T20:46:22.934773+00:00
updated_at: 2026-07-10T20:46:22.934773+00:00
---

## 背景（実測）

メモリ調査（triage-memory-usage セッション、2026-07-11）で、ホストに **ppid=1 の孤児 `usagi daemon serve` が 30 プロセス・計 ≈231MB** 残留しているのを確認した。全て tempdir ソケット（`$TMPDIR/.tmpXXXX/daemon/sock`）で listen しており、`cargo test` / `cargo llvm-cov` の 2 ビルドプロファイルから対で生まれ、6 時間以上生存していた。**テストを走らせるたびに蓄積する**実質的なリークである。

## 原因（コード調査で確定）

1. **`daemon stop` はプロセスに signal しない**。`daemon_store::request_stop` が stop マーカーファイルを書くだけで即 return し、daemon 側は serve ループ（`src/main.rs` の `run_daemon_serve`）が 500ms ポーリング（`take_stop_request`）で検知して自主終了する設計。
2. `tests/daemon_ipc_test.rs` の 4 テストは実バイナリで daemon を detached 起動（`daemon_cmd(home, "start")`）し、`daemon_cmd(home, "stop")` の後に**daemon の終了を待たずに** TempDir（`$USAGI_HOME`）を drop する。daemon が次のポーリングでマーカーを見る前にディレクトリごと消えると、`daemon_store::take_stop_request` は消えたパスに対し `Ok(false)` を返し続け、daemon は**永久に残留**する（`monitor_tick` のエラーも log して swallow）。
3. `wait_for(&sock, 10s)` の assert が `catch_unwind` の**外**にあるため、ソケットが現れずタイムアウトすると `stop` に到達せず panic → 確実に孤児化。
4. daemon 自身に idle timeout・「自分の data dir が消えたら終了する」自衛・孤児 reap の仕組みが無い。

## やること

- **テスト側（必須）**: `tests/daemon_ipc_test.rs` で
  - `daemon stop` の後、daemon の pid（`daemon.json` の記録）が実際に消えるまで待ってから TempDir を drop する。teardown で待ちがタイムアウトしたら記録 pid へ SIGKILL する。
  - `wait_for(sock)` の assert を `catch_unwind` 内へ移し、どのパスでも stop/kill が走るようにする。
- **daemon 側（恒久対策）**: serve ループで自分の daemon dir（または `$USAGI_HOME`）の消失を shutdown 条件として扱う（`take_stop_request` が「dir が無い」を `true` 相当として返す、等）。これで「所有者が消えた daemon は自死する」が成立し、テスト以外の経路（セッション worktree 削除など）でも孤児化しない。
- （任意）`daemon stop` が記録 pid へ signal も送る・終了を待つオプションを検討する。

## 確認方法

- `cargo test` / `cargo llvm-cov` を連続実行しても `pgrep -f "usagi daemon serve"` が増えないこと。
- daemon の data dir を削除すると、稼働中の daemon が 1 ポーリング周期内に自主終了すること。
- 既存の daemon start/stop/status の挙動（正常系）が変わらないこと。カバレッジ 100% 維持。
