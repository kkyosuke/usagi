---
number: 551
title: fix(tui): Home frame loop から同期 daemon RPC を排除する
status: in-progress
priority: high
labels: [review, v2, tui, ipc, performance, scheduler, responsiveness]
dependson: []
related: [506, 508, 521, 523, 527, 553, 554, 556]
created_at: 2026-07-25T22:56:07.577277+00:00
updated_at: 2026-07-26T00:17:57.987080+00:00
---

## 問題・根拠（コード調査で確定）

Home の frame loop（`crates/tui/src/presentation/mod.rs` の `drive_workspace_controller`）は、tick ごとに daemon への同期 RPC を **描画スレッド上で** 2 本発火する。tick 間隔は合成ルート（`src/runtime/tui.rs` の `run_in_terminal`）が `EventPump::new(..., Duration::from_millis(16), ...)` で与える 16ms = 約 62.5Hz である。

- `read_key` が `Key::Other` を返すと `backend.dispatch(Effect::RefreshDecisions { .. })` を実行する。`DaemonBackend` は `Effect::RefreshDecisions` を `self.decisions.refresh(workspace, self.completions())` へ渡すだけで worker を持たないため、production 実装（`src/runtime/tui.rs` の `ProductionDecisionPort::refresh` → `DaemonDecisionCommandPort::refresh`）が**そのまま描画スレッドで完走する**。`DaemonDecisionCommandPort::client` は毎回 `crate::runtime::daemon::policy_client(ClientPolicy::tui())` を新規に張る。
- 同じ tick で `tick_session_refresh` が `Effect::RefreshSessions` を返し、`ControllerHostAction::Refresh` → `begin_session_command(ui, SessionCommand::List, ..)` が **`std::thread::spawn` を実行する**。`ui.active_session_command` による capacity 1 の admission で in-flight は 1 本だが、完了ごとに次の tick が新しい OS スレッドを作る。この worker も `policy_client` を新規に張り、`session_snapshot_result` の中で `load_workspace_state(&workspace.path)` のファイル読みを行う。

### 1 回の接続確立コスト

`policy_client` → `bootstrap_client` は steady state（data dir・lock file が既存、rollover なし）でも毎回次を実行する。

| 段 | 内容 |
|---|---|
| 1 | `acquire_bootstrap_lock` → `ensure_private_dir_all(data_dir)` + `ensure_private_dir(daemon_dir)` |
| 2 | `open_private_lock` が内部でもう一度 `ensure_private_dir(parent)` |
| 3 | `lock_private_exclusive` = `FileExt::lock_exclusive`（`bootstrap.lock`、**タイムアウト無し**） |
| 4 | `std::env::current_exe()` |
| 5 | `connect_client` = `current.json` locator 読み → `connect_current` → `daemon.json` 読み → `peer_pid` → `ExactProcessControl.observe` → handshake |
| 6 | request → close |

1・2 の `ensure_private_dir(child)` は `child` ではなく **その親ディレクトリの fd を `FileExt::lock_exclusive` する**（`crates/daemon/src/infrastructure/unix_transport.rs` の `lock_setup_directory`）。したがって **1 回の bootstrap が data dir 自身に対する無期限 exclusive flock を 2 回、`bootstrap.lock` に 1 回取る**。data dir は machine 全体で共有されるため、この直列化は同一 workspace の他 client（MCP server / CLI / rollover）とも競合する。

steady state のコストを、上記の syscall 列を std のみで再現した replica（`rustc -O`、macOS、warm cache、handshake は trivial echo）で測った下限値。**実ビルドの計測ではなく floor** である（daemon 側の negotiate 処理と `UserDecision::List` / `Session::List` の実処理を含まない）。

| 条件 | `acquire_bootstrap_lock` | bootstrap 1 周 |
|---|---|---|
| 単独 | 0.26 ms | 0.40 ms |
| 4 プロセス並行 | 0.70 ms | 0.87 ms |

62.5Hz × 2 レーンでは、この floor だけで **毎秒約 125 回の bootstrap、約 50 ms/秒（1 コアの約 5%）の描画スレッド時間**を消費する。並行 client がいれば flock 待ちで比例して伸びる。

### 派生（同じ根から出る 3 つ）

- **resize が同じ洪水を起こす**: `CrosstermTerminal::read_key` は `RuntimeEvent::Resize` に対しても `RuntimeEvent::Tick` と同じ `Key::Other` を返す（`src/runtime/tui.rs`）。frame loop は両者を区別できないため、ウィンドウのドラッグリサイズ中は resize イベントごとに上記 2 本が発火する。
- **metrics の自己 channel 往復**: frame loop は `metrics_backend.poll(&metrics_sessions)` を呼び、**同じ frame 内で** `metrics_backend.drain_events()` して読み戻す。`MetricsBackend::poll`（`crates/tui/src/presentation/metrics.rs`）は `port.latest()` / `port.git_diffs()` を同期呼び出しして mpsc に送るだけなので、drain の seam は frame 予算を一切 bound しない（module doc も「a per-frame poll preserves the legacy behaviour」と書いており、非同期化は主張していない。問題は seam があるのに同期 IO が残っていることである）。`DaemonMetricsPort::latest` は 1 秒スロットルだが、その 1 回も `policy_client` 新規接続 + 描画スレッド同期である。`git_diffs` は既に 1 秒間隔の worker thread に載っている。
- **daemon 不在時の増幅**: `bootstrap::connect_or_start`（`src/runtime/bootstrap.rs`）は connect が `NotFound` のとき `run_lifecycle(exe, "start")` で**サブプロセスを起動して `.status()` で待ち**、続けて `wait_for_ready` が `READINESS_ATTEMPTS = 40` × `READINESS_DELAY = 50ms` = **最大 2 秒 `thread::sleep` する**。これが描画スレッド上で起きるため、daemon が落ちている間は 1 tick で最大 2 秒 UI が固まり、その間 bootstrap flock を握り続けて他 client も止める。これは「負荷が高い」ではなく明確な UI freeze である。

## 既存 issue との境界

- [#527](527-perf-tui-terminal-polling-ui-loop-foreground-cadence.md)（done）は **foreground terminal の `Resume`** を UI loop から外し、`src/runtime/terminal_pump.rs` の背景 pump へ移した。`poll_terminal` は現在 `self.pump.take(..)` の非ブロッキング drain である。本 issue が扱う decision / session / metrics の 3 レーンは #527 の対象外であり、いずれも今も描画スレッド上の同期 RPC である。#527 が確立した「専用永続接続 + 背景 worker + 非ブロッキング drain + cadence backoff」の形が、本 issue の目標形でもある。
- [#521](521-fix-ipc-clientpolicy-request-deadline-reconnect-budget.md)（done）は `PolicyClient` で per-request deadline を実効化した。本 issue はその deadline が効いている経路でも「毎 tick 新規接続」自体が過大であることを扱う。無期限 lane（attach/input）と bootstrap flock の bounding は別 issue が扱う。
- [#523](523-fix-tui-shared-terminal-connection-epoch-pane-subscription.md)（done）の shared connection epoch は terminal lane の subscription 契約であり、本 issue の decision / session / metrics lane には subscription が無い。

## 相互増幅（同じ描画スレッドで直列になる）

本 issue と [#553](553-fix-ipc-tui-attach-input-lane-request-deadline-bootstrap-lock.md) / [#554](554-perf-tui-frame-io.md) は独立に見えて相互に増幅する。すべて **1 本の描画スレッド上で直列**に実行されるためである。

```
daemon 側が lock を長く保持
  → #553 の無期限 attach/input lane が待つ（1 キー入力で UI 停止）
  → 同じ frame の本 issue の tick RPC も待つ（bootstrap flock も待つ）
  → #554 の read_dir と frame 構築がその後ろに並ぶ
  → 描画・入力・quit がまとめて止まる
```

さらに本 issue の「毎秒約 125 回の bootstrap」は #553 の bootstrap flock を毎秒 125 回取得することでもあるため、**他プロセス（MCP server / CLI）の接続確立も同じ頻度で直列化する**。

daemon 側 hot path（lock 保持時間そのもの）は別の triage session が起票する daemon 側 issue が扱う。番号が判明したら相互に `related` へ入れる。

## やること

- **不変条件を立てる**: 描画スレッドは daemon を同期で叩かない。frame loop に残るのは「非ブロッキング drain → 純粋な projection → draw → input」だけにする。
- decision / session / metrics の 3 レーンを、`terminal_pump` と同じ形へ寄せる。
  - レーンごとに **専用の永続接続**を持ち、tick ごとの `bootstrap_client` を廃止する。
  - **背景 worker** が cadence に従って fetch し、frame loop は非ブロッキング drain だけを行う。
  - 同一種の未処理要求は **coalesce** する（最新 1 件に畳む）。
  - **bounded cadence 250ms〜1s** を上限とし、入力等のイベントで即時 wake できるようにする（`TerminalPollPump::wake` と同じ形）。
- tick と resize を frame loop が区別できるようにし、resize が inventory RPC を誘発しないようにする。
- worker あたり 1 スレッドの常駐化により、tick ごとの `std::thread::spawn` を廃止する。
- daemon 不在時は cold-start / readiness 待ちを背景 worker 側へ移し、bounded retry + backoff にする。描画スレッドが `run_lifecycle` や `wait_for_ready` を実行しない。

## 設計上の判断が必要な点

- **cadence を誰が決めるか**。3 レーンは要求の性質が違う（decision は通知性が要る、session lifecycle は他 client の変更追随、metrics は表示専用）。1 つの scheduler が 3 レーンを回すのか、レーンごとに独立 worker を持つのかを決める。永続接続を 3 本張るコストと、1 本を共有して head-of-line blocking を許すコストの比較が判断点である。
- **coalesce の可視的な意味**。session refresh を畳むと、他 client が作った session の反映が最大 cadence 分遅れる。どの遅延なら受け入れるか（および `RefreshSessions` の revision 調停との整合）を先に決める。
- **daemon cold-start の権限**。現在は最初に接続した任意の lane が daemon を起動しうる。背景 worker へ移すなら「どの lane が cold-start してよいか」を 1 か所に決める必要がある（display-only の metrics は `observation_client` が既に「起動しない」を選んでいる。この方針を他レーンにも広げるか）。
- **resize と tick の区別**。`Key::Other` の分岐を増やすか、`AppEvent::Resize` の既存経路（frame loop 先頭で毎回 apply している）に寄せて `Key::Other` から resize を消すか。後者なら reducer 側の resize 契約に触るため、`document/03-tui.md` の「画面と入力」との整合を確認する。
- **metrics seam の去り先**。`MetricsBackend` の自己 channel を残して port 側を非同期化するのか、`MetricsBackend` ごと背景 worker にするのかを決める。`GitDiff` が presentation 型であるため `controller::BackendEvent` へは載せられない（現行 module doc の制約）ことを前提にする。

## 受入条件

- idle な Home 画面で、描画スレッドから発行される daemon request が 0 件である（すべて背景 worker 発行）。
- idle 時の request rate と接続確立回数が、レーンごとに設定した cadence 上限を超えない。
- slow / hung な refresh が draw / input / modal / quit を block しない。
- daemon 不在時に描画スレッドがサブプロセス起動や readiness sleep を実行しない。cold-start は bounded retry + backoff で背景側だけが試みる。
- ウィンドウのドラッグリサイズ中に decision / session の inventory RPC が発火しない。
- tick ごとの `std::thread::spawn` が無い（worker は常駐）。
- カバレッジ 100% を維持する。`document/03-tui.md` を更新する（本 issue を実装する側が行う）。

## 必須回帰テスト・計測

- **fake clock + fake port** で、N 秒相当の tick を進めたときの (a) request 件数 (b) 接続確立回数 が cadence 上限以下であることを assert する。tick 数に比例しないことを固定する。
- fake port が応答を返さない（hung）状態で、frame loop が draw / input / modal open / quit を規定 frame 数以内に完了することを assert する。
- coalesce: cadence 1 周期に複数の refresh 要求を積んでも、発行される request が 1 件であることを assert する。
- resize イベント列（`Key::Other` ではなく resize として与える）で decision / session request が 0 件であることを assert する。
- daemon 不在の fake で、描画スレッド側の lifecycle spawn 回数が 0、背景側の retry が bounded であることを assert する。
- 常駐 worker のスレッド数が tick 数に依存しないことを assert する。
- 実測は本 issue 記載の syscall floor を基準線とし、実装後に同じ条件で「idle 時の毎秒 bootstrap 回数」を再計測して記録する。
