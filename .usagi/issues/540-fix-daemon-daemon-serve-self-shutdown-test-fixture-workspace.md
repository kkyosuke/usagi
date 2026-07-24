---
number: 540
title: fix(daemon): 権威を失った daemon serve を self-shutdown させ、test 起動を fixture workspace へ隔離する
status: todo
priority: high
labels: [v2, daemon, test, lifecycle, leak]
dependson: []
related: [171]
created_at: 2026-07-24T22:38:25.106198+00:00
updated_at: 2026-07-24T22:38:25.106198+00:00
---

## 問題・根拠（実測）

v2 の `usagi daemon serve` が孤児化して残留し、coordinator 環境で最大 20 プロセス（7/19 起動のものを含む）が生存していた。調査時点で生存していた 1 件を直接観測した結果、残留の実体が判明した。

```text
PID 25529  PPID 1  ELAPSED 01:48:04
  /Users/.../.usagi/sessions/issue-534/target/llvm-cov-target/debug/usagi daemon serve
  USAGI_HOME=/tmp/usagi-sw0yST          ← test の tempdir（すでに削除済み）
  CARGO_BIN_EXE_usagi=.../issues-534/target/llvm-cov-target/debug/usagi
  LLVM_PROFILE_FILE=.../llvm-cov-target/issue-534-%p-%m.profraw
  cwd = /Users/.../.usagi/sessions/issue-534   ← 開発者の live session worktree
  fd 3u = /private/tmp/usagi-sw0yST/local/daemon/daemon.lock
```

つまり残留 daemon は **coverage 実行中の integration test が起動した daemon** であり、次の 3 点が重なって永久に生き残る。

1. **daemon 自身に自衛が無い**。`serve` の shutdown 条件は SIGINT / SIGTERM（`SignalShutdown::wait`）と IPC 由来の `shutdown: AtomicBool` だけである。`src/runtime/daemon.rs` / `crates/daemon/src/usecase/serve.rs` に idle timeout、data dir 消失検知、親プロセス死亡検知はいずれも存在しない。したがって `$USAGI_HOME` の tempdir が消えても、socket の親 directory が消えても、daemon は listen し続ける。
2. **detached 起動なので launcher の死に連動しない**。`ServeLauncher::launch` は `process_group(0)` で意図的に別 process group へ切り離す（前景 hangup で死なせないための設計であり、これ自体は正しい）。結果として、test harness が SIGKILL / abort / Ctrl-C で死ぬと reap する者がいない。
3. **単一インスタンス fence が蓄積を止められない**。fence は `<data_dir>/daemon/daemon.lock` の `flock` であり（`FileInstanceLock`、`run_inner`）、test は 1 件ごとに一意な tempdir を `$USAGI_HOME` に渡す。よって全 daemon が**別々の lock file** を取り、互いを排除しない。蓄積は無制限である。

さらに **test が daemon の workspace root を fixture ではなく開発者の実 worktree に束縛している**。daemon の workspace root は起動時 cwd（`spawn_ipc_server` の `std::env::current_dir()`）で決まる。`tests/cli_tui.rs` の `ProductionDaemonCleanup::spawn` と `daemon start` / `daemon restart` を叩く各 test は `.current_dir()` を設定しないため、cwd は cargo の manifest dir、すなわち **その session worktree のルート**になる。上記 PID 25529 の cwd がまさにそれである（`tests/agent_ipc_e2e.rs` の `start_daemon` と `tests/support/mcp.rs` は正しく fixture repo を `.current_dir()` している）。

これは二次被害を生む。残留 daemon が session worktree 内に cwd を持ち、`target/llvm-cov-target/debug/usagi` を実行中のまま握るため、その session の `git worktree remove` / `remove_dir_all` が失敗・停滞する。「巨大 target の session remove が数分ブロックする」症状（[#529](529-fix-daemon-failed-session-remove.md) 周辺、および本 issue と同時に起票した teardown worker の issue）の一因である。

なお [#171](171-fix-daemon-usagi-daemon-serve-teardown-data-dir-self-shutdown.md)（`done`）は、当時の daemon 実装に対してまったく同じクラスの不具合（ppid=1 の孤児 30 プロセス、tempdir socket、`cargo test` / `cargo llvm-cov` の 2 profile から対で発生）を修正し、恒久対策として「自分の data dir が消えたら終了する」自衛を要求していた。**v2 daemon（`crates/` + ルート `src/`）にはその自衛が存在しない**。同じ不具合が同じ原因で再発している。

## 設計判断

正本は [document/proposals/13-daemon-singleton-and-teardown.md](../../document/proposals/13-daemon-singleton-and-teardown.md)。要点は次のとおり。

- 採用する終了条件は **custody（権威）の喪失**であり、idle timeout ではない。正当な daemon は client が 0 でも live PTY と supervisor scheduler を所有するため、idle は終了根拠にならない。一方 custody 喪失は「この process はもう誰の権威でもない」を意味する精密な signal である。
- custody は 2 つの invariant で判定する。
  1. **lock custody** — 保持中の lock fd の `(dev, ino)` と、lock path を `stat` した結果が一致する。path が消えた／別 inode に置き換わった場合、この process はその data dir の singleton ではない。既存の `verify_private_lock_path` と同じ検証語彙を使う。
  2. **record custody** — `daemon.json` が今もこの pid と OS の process-start identity を記録している。record 消失・別 owner は権威の retire を意味する。
- 判定は 1 秒程度の周期 tick で行い、喪失時は既存の `shutdown: AtomicBool` を立てる（SIGTERM と同じ経路）。よって endpoint retire / cleanup は通常経路を通り、data dir がすでに消えている場合の cleanup は no-op として成功しなければならない（block も panic もしない）。
- 判定ロジックは注入した port に対する純関数として `crates/daemon/src/usecase/` に置き、fake で unit test する。実 `stat` / `fstat` の薄いラッパだけが合成ルート側の real IO である（`#[coverage(off)]` を使う場合の許可理由は `real_io`）。

## やること

### daemon 側（恒久対策）

- custody probe port と判定 usecase を `crates/daemon/src/usecase/` に追加する（lock inode identity と record owner identity を返す port を注入）。
- serve の supervisor tick（`spawn_pr_refresh_worker` と同型の worker）から周期実行し、custody 喪失で graceful shutdown を要求する。
- data dir が消えている状態の endpoint cleanup / record clear が no-op で成功することを保証する。

### test 側（再発防止）

- `daemon serve` を直接・間接（`daemon start` / `daemon restart` / bootstrap 経由）に起動する全 test を、**必ず fixture workspace を `.current_dir()` に指定**して起動する。忘れられない形にするため `tests/support/` に共有 helper を置き、helper 経由以外で daemon を起こさない。
- helper は起動した daemon の workspace root が fixture であることを assert する（開発者の live worktree に束縛されていないことの回帰テスト）。
- teardown は record の pid + process-start identity で exact 一致を確認して reap し、graceful stop がタイムアウトしたら SIGTERM → SIGKILL へ段階的に落とす。`tests/cli_tui.rs` の `ProductionDaemonCleanup` は `Child` 由来なので概ね正しいが、`daemon start` / `daemon restart` の間接起動経路は reap されていない。

## 受入条件

- `cargo test --workspace` と coverage 経路を連続実行しても、実行後に `pgrep -f "usagi daemon serve"` が増えない。
- 稼働中 daemon の data dir（または `daemon.lock`）を削除すると、1 tick 周期内に自主終了する。fake port による unit test と、実プロセスを使う integration test の両方で固定する。
- test が起動する daemon の workspace root が、常に fixture workspace であって開発者の worktree ではない。
- 既存の start / stop / status / restart / replace の正常系挙動が変わらない。カバレッジ 100% を維持する。
- [document/05-daemon.md](../../document/05-daemon.md) の daemon process lifecycle に self-shutdown 条件を畳み込む。
