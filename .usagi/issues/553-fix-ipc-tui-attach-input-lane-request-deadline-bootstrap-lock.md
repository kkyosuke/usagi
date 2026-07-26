---
number: 553
title: fix(ipc): TUI の attach/input lane に request deadline を入れ bootstrap lock を有界化する
status: in-progress
priority: high
labels: [review, v2, ipc, tui, client, timeout, resilience]
dependson: []
related: [508, 517, 519, 521, 523, 551, 554, 556]
created_at: 2026-07-25T22:57:20.001062+00:00
updated_at: 2026-07-26T00:19:32.593927+00:00
---

## 問題・根拠（コード調査で確定）

### 1. attach / input lane が無期限 socket である

`src/runtime/tui.rs` の `DaemonAgentCommandPort` は 2 本の terminal lane を持ち、**deadline が入っているのは片方だけ**である。

| lane | 生成 | read deadline | write deadline | 載る操作 |
|---|---|---|---|---|
| poll lane（`poll_client`） | `crate::runtime::daemon::client(ClientPolicy::tui())` + `set_read_timeout(POLL_LANE_DEADLINE = 50ms)` | 50ms | **無し** | `Resume` / `Resize` |
| terminal lane（`terminal_client`） | `crate::runtime::daemon::client(ClientPolicy::tui())` のみ | **無し** | **無し** | `Attach` / `Input` / `InputOutcome` / `Detach` |

`crate::runtime::daemon::client` は `client_for` → `bootstrap_client(|data_dir, build| connect_client(..))` で、`connect_client` が返すのは**生の `std::os::unix::net::UnixStream`** である（`src/runtime/daemon.rs`）。`policy_client` の `DeadlineStream<SystemClock, DeadlineUnixStream>` 経路を通らないため、`ClientPolicy::tui().timeout_ms = 2000` も `reconnect_attempts = 3` も **この lane には一切効かない**。root の理解どおりである。

この lane を叩くのは描画スレッドである。

- `input_terminal` は `forward_live_terminal_input` 経由で frame loop から同期に呼ばれ、`terminal_input_is_durable()`（= `terminal_client()`）と `client.request(DaemonRequest::Terminal { action: Input, .. })` を実行する。**1 キー入力で UI が無期限停止しうる**。
- `attach_terminal` も同じ lane で、`terminal_client()?.terminal_snapshot_mode()` と `terminal_request(Attach, ..)` を描画スレッドで実行する。tab 切替 / reconnect / restore 適用が同じ経路に載る。
- `resize_terminal` は `poll_request` を使うため 50ms で戻る。つまり **同じ frame の中に、有界な lane と無期限な lane が混在している**。

### 2. bootstrap lock が無期限ブロックする

`acquire_bootstrap_lock`（`src/runtime/daemon.rs`）は `lock_private_exclusive` → `FileExt::lock_exclusive` で、**タイムアウトも `try_lock` も持たない**。さらに内部の `ensure_private_dir(child)` は `child` の**親ディレクトリの fd を `FileExt::lock_exclusive` する**（`crates/daemon/src/infrastructure/unix_transport.rs` の `lock_setup_directory`）。したがって 1 回の bootstrap が

1. data dir 自身への無期限 exclusive flock（`ensure_private_dir(daemon_dir)`）
2. 同じものをもう 1 回（`open_private_lock` が内部で再度 `ensure_private_dir(parent)`）
3. `bootstrap.lock` への無期限 exclusive flock

を取る。data dir は machine 全体で共有されるため、**MCP server / CLI / rollover のいずれかが bootstrap 区間にいる間、UI 経路の接続確立が無期限に待つ**。`bootstrap_client` は `connect_or_start` を lock 保持中に呼ぶので、lock を握った側が daemon cold-start（サブプロセス起動 + `wait_for_ready` 最大 40 × 50ms = 2 秒。`src/runtime/bootstrap.rs`）に入ると、その 2 秒がそのまま他プロセスの待ち時間になる。

## 既存 issue との境界

- [#521](521-fix-ipc-clientpolicy-request-deadline-reconnect-budget.md)（done）は `PolicyClient` を導入し、`policy_client` / `observation_client` 経路で attempt 単位の end-to-end deadline と `reconnect_attempts` を実効化した。**#521 が閉じていない残余は 2 つ**である。
  1. `crate::runtime::daemon::client`（生 `UnixStream`）を使う terminal lane は `PolicyClient` を通らないため、#521 の受入条件「timeout 後も TUI draw/input/quit が無期限停止しない」が **この lane では満たされていない**。
  2. #521 の retry eligibility table は「terminal input: #519 完了までは retry 不可」と定義しており、terminal input を意図的に scope 外に置いた。**[#519](519-feat-ipc-terminal-input-ack-loss-cross-connection-replay.md) は done** であり、`terminal_input_outcome` による cross-connection の outcome 解決が既に存在する。したがって「deadline を入れると ACK 喪失が未解決になる」という当時の制約は解消済みで、input lane を有界化する前提が揃っている。
- [#523](523-fix-tui-shared-terminal-connection-epoch-pane-subscription.md)（done）は、この lane を drop したときに全 pane の subscription を epoch で再確立する契約を持つ。deadline 導入は「timeout を transport failure として lane を drop する」設計を取りうるため、#523 の epoch 契約を壊さないことが制約になる（`related`）。
- [#508](508-fix-tui-ipc-draining-generation-inventory-terminalref-owner-routing.md)（todo）は draining generation の inventory / owner routing であり、lane の deadline とは独立（`related`）。

## poll lane にだけ対処が入っている理由（調査結果）

意図的である。`poll_client` のコメントが根拠を明示している。`Resume` / `Resize` は daemon 側で stateless（terminal id と offset だけで決まる）ため、この lane は attach せず input subscription も持たない。read timeout で socket 上に未読 frame が残っても `poll_request` が lane ごと落とせばよく、失うものが無い。

対して terminal lane は **exactly-once の input ledger を載せている**。ここで素朴に read timeout を入れると (a) 未読 frame が残った socket は再利用できず (b) lane を drop すると daemon がそのコネクションの全 attachment を解放するため、**1 pane の timeout が全 pane の subscription を落とす**（#523 が閉じた cascade と同じ形）。つまり「timeout を入れなかった」ことに理由はあるが、その理由は **#519 の cross-connection replay が無かった時点の理由**である。現在は `input_terminal` が `TerminalError::InputEffectUnknown` を返し、`terminal_input_outcome` が durable ledger へ問い合わせて `Final` / `Unknown` を確定できる。したがって「安全だから無期限のままでよい」ではなく、**前提が変わったので有界化すべき**が結論である。

## やること

- terminal lane に **attempt 単位の end-to-end deadline** を入れる。`policy_client` の `DeadlineStream` 相当を使うか、`client()` に deadline を armed にした変種を用意するかを決める（下記判断点）。
- deadline 超過を `InputEffectUnknown` として扱い、`terminal_input_outcome`（#519 の durable ledger）で解決させる。**blind retry はしない**。
- `attach` の deadline 超過は attach 失敗として扱い、epoch を進めた上で reattach 経路に乗せる（#523 の契約に従う）。
- 1 pane の timeout が他 pane の subscription を落とさないようにする。lane drop が必要なら、drop 後の再確立が #523 の epoch 経路で完結することをテストで固定する。
- `acquire_bootstrap_lock` を **`try_lock` + bounded retry + typed error** に変える。`ensure_private_dir` 系のディレクトリ flock も同じ扱いにするか、少なくとも UI 経路が無期限に待たない形にする。
- bootstrap lock 保持区間から、長時間かかる処理（daemon cold-start の `run_lifecycle` + `wait_for_ready`）を切り離せるかを検討する。切り離せない場合は「lock を握る最大時間」を明示し、待ち側の budget をそれに合わせる。

## 設計上の判断が必要な点

- **write deadline をどう扱うか**。現在どちらの lane にも write timeout が無い。daemon が read を止めて socket buffer が埋まると `client.request` の write 側で止まる。read だけ有界にしても UI freeze は残るため、write も含めるかを決める。
- **timeout 後の socket 再利用**。未読 frame が残った socket は position 不明なので再利用できない。lane を drop する（= epoch を進める）以外の選択肢（frame 境界までの drain、request id による late response 破棄）を取るかを決める。#521 の「late response は新 request へ誤相関させない」制約に従う。
- **deadline の値**。`ClientPolicy::tui().timeout_ms = 2000` を input lane にそのまま適用すると、最悪 2 秒 UI が止まる。poll lane の 50ms は短すぎる（input は daemon 側の PTY write を伴う）。input / attach それぞれの妥当な budget を決める必要がある。これは frame 予算の問題でもあるため [#551](551-fix-tui-home-frame-loop-daemon-rpc.md) の「描画スレッドは daemon を同期で叩かない」不変条件と併せて決めるのが望ましい。
- **bootstrap lock の bounded retry の失敗表現**。`ClientError::Unavailable` に畳むと「daemon が無い」と区別できない。typed error を新設して「別 client が bootstrap 中」を UI に出すかを決める。
- **ディレクトリ flock の必要性**。`ensure_private_dir` が親ディレクトリを exclusive flock するのは crash residue の修復を直列化するためである。steady state（すべて既存・正しい mode）で flock を省けるか（先に検証だけして、修復が必要なときだけ lock を取る）を検討する。省ければ bootstrap の主要コストが消える。

## 受入条件

- daemon が応答を止めた状態で、キー入力 1 回・tab 切替 1 回が規定の budget + 小さな scheduler 誤差以内に戻る。UI は draw / input / quit を続行できる。
- deadline 超過した input が blind retry されず、`terminal_input_outcome` で `Final` / `Unknown` に収束する。PTY への write 適用回数が **exactly 1**（または 0）である。
- 1 pane の timeout が他 pane の subscription を無効化しない。lane drop が起きた場合は #523 の epoch 経路で全 pane が再確立する。
- 別プロセスが bootstrap lock を保持している間、UI 経路が bounded な時間で typed error に戻る（無期限に待たない）。
- `client()` 経路に deadline が入ったことが type / 構成上で保証され、無期限 socket を新規に作れないことがテストで固定されている。
- カバレッジ 100% を維持する。`document/04-ipc.md`（surface 別 deadline と lane 契約）を更新する（本 issue を実装する側が行う）。

## 必須回帰テスト・計測

- fake clock + deadline transport で、terminal lane の (a) hello stall (b) write stall (c) 無応答 (d) partial response の 4 fixture が budget 内で戻ることを assert する。
- response loss fixture（daemon が PTY へ適用してから ACK を partial write して切断）で、PTY write 回数 = 1、client retry 回数 = 0、outcome が `InputEffectUnknown` → `terminal_input_outcome` で `Final` に収束することを assert する。
- request frame の partial write で daemon dispatch 前に切れた fixture で、effect 回数 = 0、retry 回数 = 0 を assert する。
- 複数 pane fixture で、1 pane の timeout 後に他 pane の subscription が有効なまま（または epoch 経路で再確立済み）であることを assert する。
- bootstrap lock を別プロセスが保持した状態で、`client()` / `policy_client()` が bounded な時間で typed error を返すことを assert する（`try_lock` の retry 回数と上限時間を固定）。
- 実 PTY E2E（`tests/cli_tui_pty.rs` 系）で、hung daemon 中の quit wall-clock bound を固定する。既存の直列化 lock を使う（`document/06-conventions.md` の「重い E2E の直列化」）。
