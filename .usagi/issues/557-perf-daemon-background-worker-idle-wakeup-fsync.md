---
number: 557
title: perf(daemon): background worker の idle wakeup と無条件 fsync を削る
status: in-progress
priority: medium
labels: [review, v2, daemon, runtime, performance]
dependson: []
related: [518, 555]
created_at: 2026-07-25T23:00:31.152370+00:00
updated_at: 2026-07-26T06:26:28.346217+00:00
---

## 問題・根拠（コード調査で確定）

daemon の background worker は tick 待ちを `10 ms sleep` のスピンで実装している。shutdown へ即応するためだが、
**worker 数 × 毎秒 100 回の timer wakeup**になり、何もしていない daemon が常時 CPU と電力を使う。加えて
decision maintenance worker は毎 tick で**無条件に fsync 付きの atomic write** を行い、shutdown を一切観測しない。

### sleep スピンで tick を待つ worker（`src/runtime/daemon.rs`、行番号は起票時点）

| worker | 定義 | tick 定数 | 待ち方 | idle wakeup |
|---|---|---|---|---|
| PR refresh | `spawn_pr_refresh_worker`（1503 行） | `PR_REFRESH_TICK` = 250 ms | `while !shutdown… && Instant::now() < deadline { sleep(10 ms) }`（1545 行） | 100 / 秒 |
| daemon custody | `spawn_custody_worker`（1631 行） | `CUSTODY_TICK` = 1 s | 同（1667 行） | 100 / 秒 |
| retention GC | `spawn_retention_gc_worker`（1703 行） | `RETENTION_GC_TICK` = 30 s | 同（1717 行） | 100 / 秒 |
| IPC accept | `start_ipc_accept_loop`（1997 行） | なし | non-blocking `accept()` が `WouldBlock` / `Err` で `sleep(10 ms)`（2092 / 2094 行） | 100 / 秒 |
| lifecycle owner の shutdown 待ち | `DaemonSignals::wait`（4969 行） | なし | `shutdown` flag と `signals.pending()` を `sleep(10 ms)` で polling | 100 / 秒 |
| decision maintenance | `start_decision_maintenance`（1776 行） | なし（`sleep(250 ms)` 直書き） | `loop { … sleep(250 ms) }`。**shutdown を見ない** | 4 / 秒 |

**合計で idle の daemon が毎秒 約 504 回 wakeup する**。`RETENTION_GC_TICK` は 30 秒に 1 回しか仕事をしないのに、
その 30 秒間に 3,000 回 wakeup している。

### root の指摘のうち成立しなかった項目

**session teardown worker は既に Condvar である**。`spawn_session_teardown_worker`（1575 行）は
`signal.wait(tick)` を使い、その実体は `crates/daemon/src/usecase/session_teardown.rs` の `TeardownSignal`
（`Mutex<bool>` + `Condvar::wait_timeout_while`）である。sleep スピンではないので**修正対象に含めない**。
むしろこれが本 issue の**参照実装**であり、他の worker はこれと同じ形へ寄せる。

### decision maintenance worker の追加欠陥

`start_decision_maintenance` は 250 ms ごとに `decisions.expire_due(now)` と
`consume_user_decision_events(&decisions)` を呼ぶ。

- `UserDecisionStore::expire_due` は `crates/core/src/infrastructure/store/user_decision.rs` の `mutate` を通る。
  `mutate` は `StoreLock::acquire(&self.dir)`（`<data-dir>/daemon/.lock` の flock）→ `load()` →
  **変化の有無に関わらず `json_file::write_atomic`** の順で動く。`write_atomic` は temp 作成 + `sync_all`（fsync）+
  rename + 親ディレクトリの fsync である。
- `consume_user_decision_events` は `decisions.events()` → もう 1 回の `load()`。

したがって **decision が 0 件でも、idle の daemon が毎秒 4 回の fsync 付き書き込み・8 回の JSON 読み込み・
4 回の flock 取得を永久に行う**。SSD の書き込み寿命と省電力の両方に効き、`<data-dir>/daemon` の store lock を
握るので他の decision 経路とも競合する。

- さらにこの worker は `shutdown` を観測しないため、custody 喪失や SIGTERM で daemon が終了処理に入っている間も
  同じディレクトリへ書き続ける。custody 喪失 shutdown は「解放しつつあるツリーを再作成しない」ことを意図している
  （[document/05-daemon.md](../../document/05-daemon.md#custody-喪失による-self-shutdown)）が、この worker は
  その意図の外にいる。

## 既存 issue との境界

- [#555](555-perf-daemon-pr-identity-pty-hot-path.md)（出力 hot path の PR 抽出）とは**性質が違い、独立にマージできる**。
  あちらは「出力量に比例して lock 保持時間が伸びる」問題、本 issue は「何もしていないときの定常コスト」である。
  #555 が新しい抽出 worker を足すので、その worker の shutdown 待ちは本 issue の方針（Condvar）に従わせる。
- [#518](518-refactor-daemon-owner-generation-runtime-shard-global-resource-allocator.md) は worker の
  **所有権と generation fence** を扱う。本 issue は worker の**待ち方**だけを変え、どの worker が何を所有するかは
  変更しない。
- retention GC の**予算・eviction 順序・typed expiry** は
  [#526](526-fix-daemon-terminal-agent-tombstone-retention-aggregate-bound-gc.md)（done）が正本であり、
  本 issue は `RETENTION_GC_TICK` の待ち方と妥当性だけを見る。

## やること

1. `PR_REFRESH_TICK` / `CUSTODY_TICK` / `RETENTION_GC_TICK` の 3 worker の tick 待ちを、`TeardownSignal` と同じ
   `Condvar::wait_timeout` 相当（shutdown を notify する共有 signal）へ置き換える。shutdown 即応と idle 0 wakeup を
   同時に満たす。
2. IPC accept loop の `WouldBlock` sleep を廃し、shutdown で起こせる blocking accept にする。
3. lifecycle owner の `wait()` を、`shutdown` flag と signal の両方で起きられる待ちにする。
4. decision maintenance worker に shutdown 待ちを入れ、**変化が無いときに durable write をしない**ようにする。
5. 各 tick 定数の妥当性を見直し、**根拠をコメントまたは [document/05-daemon.md](../../document/05-daemon.md) に書く**
   （現状 `CUSTODY_TICK` と `SESSION_TEARDOWN_TICK` には根拠コメントがあるが、`PR_REFRESH_TICK` /
   `RETENTION_GC_TICK` には無い）。

## 設計上の判断が必要な点

- **shutdown signal を 1 つにまとめるか、worker ごとに持つか**。`Arc<AtomicBool>` + 1 つの共有 `Condvar` に
  `notify_all` する形が最小だが、その場合 shutdown 以外の理由で起こす（PR refresh を即時に走らせる等）需要が
  将来出たときに worker が混ざる。`TeardownSignal` は「teardown 用」に専用化されているので、同じ粒度で
  worker ごとに signal を持たせるか、shutdown 専用の 1 つにするかを決める。
- **accept loop の起こし方**。`Condvar` は使えない（`accept()` は fd を待つ）。選択肢は
  (a) blocking accept にして shutdown 時に自分のソケットへ 1 本接続して起こす、
  (b) listener fd と shutdown 用 pipe を `poll(2)` で同時に待つ。
  (a) は接続を受けた側が「shutdown 用の接続」を識別して捨てる必要があり、`SecureUnixListener` の peer 検証と
  どう噛むかを決める。(b) は fd を 1 本増やすが識別が要らない。**listener の non-blocking 設定と
  `SecureUnixListener` の契約を変えずに済むのはどちらか**を確認して選ぶ。
- **lifecycle owner の待ち方**。`signal-hook` の handler から `Condvar` を notify するのは async-signal-safe ではない。
  `signal_hook` の pipe / `SignalsInfo` の blocking iterator と、accept worker の exit guard が立てる
  `shutdown` flag の両方を 1 か所で待つ必要がある。現在の polling はこの 2 経路を 1 か所で待つための素朴な解であり、
  置き換えるなら pipe 経由に寄せるのが自然。**worker spawn 前に signal handler を用意する現在の順序
  （[document/05-daemon.md](../../document/05-daemon.md#daemon-process-lifecycle)）を崩さない**こと。
- **`mutate` の「変化なしなら書かない」をどこに入れるか**。`mutate` は共有ヘルパなので、そこを変えると
  `UserDecisionStore` の他の呼び出し元にも効く。選択肢は (a) `mutate` のクロージャに「変化した」を返させて
  write を条件付きにする、(b) `expire_due` 側で「期限切れ候補が 0 件なら `mutate` を呼ばない」read-only fast path を
  置く。(a) は全呼び出し元に効くが、`mutate` の「read-modify-write を 1 つの lock で」という契約の意味を変える。
  (b) は局所的だが、読み込みは残る（1 read / 250 ms）。**どちらを選ぶか根拠付きで決める**。
- **tick 定数の妥当性**。見直しの根拠を明示する。
  - `PR_REFRESH_TICK` 250 ms: 1 tick あたり最大 2 identity、freshness 60 秒。250 ms tick は「PR を 1 個検出した
    直後にどれだけ早く title/state が出るか」を決める。wakeup が 0 になるならこの値を変える理由は無い可能性が高い。
  - `CUSTODY_TICK` 1 s: 既存コメントに「2 回の stat は無料」と根拠がある。
  - `RETENTION_GC_TICK` 30 s: launch と exit が既に GC を駆動するので idle 専用。根拠を書く。
  - `SESSION_TEARDOWN_TICK` 1 s: 既に Condvar で即時に起きるため、tick は finalization 失敗の retry 間隔のみ。
  - decision maintenance の 250 ms: 定数化されておらず、根拠も無い。決めて定数にする。

## 受入条件

- [ ] **idle の daemon の timer wakeup が、各 worker の意図した tick の回数だけになる**（10 ms スピンが消える）。
      注入した tick と fake clock で、worker が tick 1 回につき 1 回だけ起きることをテストで固定する。
- [ ] **shutdown 応答が退行しない**。shutdown 要求から各 worker の loop 離脱までが、tick の長さに依存せず
      有界であることをテストで固定する（`RETENTION_GC_TICK` = 30 s の worker が 30 秒待たないこと）。
- [ ] IPC accept loop が shutdown で確実に起き、`ShutdownOnIpcWorkerExit` の意味論（worker が抜けたら
      lifecycle owner を起こす）が維持される。
- [ ] lifecycle owner が **signal 経由の shutdown と accept worker の exit guard 経由の shutdown の両方**で起きる。
      片方だけになる退行が無いことをテストで固定する。
- [ ] **decision が 0 件のとき、decision maintenance worker が durable write を 1 回も行わない**。
      write 回数を数える fake で固定する。
- [ ] decision maintenance worker が shutdown を観測して loop を抜ける。
- [ ] 全 tick 定数に根拠のコメントがあり、値を変えた定数は変えた理由が書かれている。
- [ ] カバレッジ 100% を維持する。[document/05-daemon.md](../../document/05-daemon.md) の該当節（daemon process
      lifecycle / custody 喪失による self-shutdown / PR refresh scheduler / session teardown worker /
      final retention と aggregate GC）の記述を実装に合わせて更新する。**未実装の契約を先に書かない**。

## 必須回帰テスト・計測

- `cargo test -p usagi --bin usagi`（`src/runtime/daemon.rs` の `spawn_*_worker` 系。tick と shutdown を注入する
  既存の test 構造をそのまま使う）
- `cargo test -p usagi-daemon`（`usecase::session_teardown` の `TeardownSignal` が退行しないこと）
- `cargo test -p usagi-core`（`infrastructure::store::user_decision`。`mutate` の write 条件を変える場合は
  他の呼び出し元の durable 挙動を固定する）
- `cargo test -p usagi --test <target>`（daemon を起動する root 結合テスト。起動は必ず
  [`tests/support/daemon.rs` の command builder 経由](../../document/06-conventions.md#結合テストからの-daemon-起動)）
- **idle コストの実測**: shutdown まで N 秒間 idle で走らせた daemon の CPU 時間（`getrusage` は
  `ProcessResourceSampler` が既に取っている）と、`<data-dir>/daemon/user-decisions.json` の mtime 更新回数を
  before / after で比較し、PR 本文に載せる。
- Rust 差分を含むため、fmt / `cargo check --workspace --all-targets` /
  `cargo clippy --workspace --all-targets -- -D warnings` / `scripts/recommend-tests.sh origin/main` の推奨 test を
  通し、full gate は PR CI で確認する。

## 実装時の決定

### shutdown signal は 1 つにまとめた

`ShutdownRequest`（`crates/daemon/src/usecase/shutdown.rs`）1 つを全 worker が共有する。flag が権威で condvar が
edge を運ぶ形にした。理由は 2 つある。

- **signal handler は flag しか書けない**。`signal_hook::flag::register` は async-signal-safe な atomic store を
  行うが condvar を notify できない。したがって flag を権威に残すのは選択ではなく制約である。
- worker ごとに signal を持たせても、現状「起こす理由」は shutdown だけである。`TeardownSignal` が専用化されて
  いるのは、そちらが shutdown ではなく **admission** で起きる必要があるからで、粒度を分ける根拠がそこにはある。
  shutdown 以外の起床理由が増えたときに分ければよい。

`wait_for_tick` は「tick 経過」と「shutdown 要求」の早い方まで park し、shutdown を要求されたかを返す。
`request()` は store の前に mutex を取るので、waiter が predicate を評価してから wait に入るまでの隙間で
通知を落とさない。

### accept loop は poll(2) + mirror pipe にした（案 b）

案 (a) の blocking accept + 自己接続は採らなかった。`SecureUnixListener` は bind 時に non-blocking を設定し、
socket identity の fence をその前提で組んでいる（#516 が書き直した領域）。blocking へ切り替えると
その契約に触るうえ、起こす側が listener を所有していない（accept worker が move で持ち、join で返す）ため
自己接続の相手先 path を composition 側で再構成する必要が出る。

案 (b) は listener に `readiness_fd()` を 1 つ足すだけで済み、non-blocking 契約も peer credential 検査も変えない。
condvar は descriptor 待ちに混ぜられないので、shutdown 要求を descriptor へ写す thread を 1 つ置いた。この thread は
condvar で park するので、idle の wakeup は増えない。`poll` の timeout は無期限にし、失敗・EINTR は
「listener ready」として扱って呼び出し側に flag を再確認させる（EINTR を shutdown と誤認しない）。

### lifecycle owner は signal を専用 thread で blocking 受信する

`wait()` は `wait_until_requested()` の park だけになった。signal は `prepare()` が起こす
`usagi-daemon-signal` thread が `signal_hook` の blocking iterator で受け、`request()` へ変換する。
thread の起動は `prepare()` の中、**worker spawn より前**なので
[daemon process lifecycle](../../document/05-daemon.md#daemon-process-lifecycle) の順序は変わらない。
`flag::register` は async-signal-safe な backstop として残す。

### `mutate` は変えず `expire_due` に read-only fast path を置いた（案 b）

案 (a)（`mutate` に「変化した」を返させる）は 6 箇所の呼び出し元すべてを書き換える必要があり、1 つ間違えると
**durable write の消失**という silent なデータ損失になる。得られるものは idle 経路以外では小さい。

案 (b) は `expire_due` の中だけで閉じる。lock を取る前に lock-free の `load()` で「期限到来が 1 件も無い」ことを
確かめ、その場合は空を返す。atomically replaced な document を lock 無しで読むのは `get` / `pending` / `events` が
既に採っている形なので、新しい前提を持ち込まない。権威ある判定は従来どおり lock の中で再度行うので TOCTOU も無い。
read は 1 tick あたり 2 回残る（expiry 判定と outbox drain）。これを削るには外部 process の書き込みを知る
invalidation が必要で、本 issue の範囲を超える。

### tick 定数は値を変えず、根拠を書いた

wakeup が tick 1 回あたり 1 回になった時点で、どの定数も「短いから高コスト」ではなくなったため、
**観測したい状態の粒度で決まる値をそのまま残した**。`PR_REFRESH_TICK` 250 ms / `CUSTODY_TICK` 1 s /
`SESSION_TEARDOWN_TICK` 1 s / `RETENTION_GC_TICK` 30 s は据え置き。decision maintenance の 250 ms は
`DECISION_MAINTENANCE_TICK` として定数化した（従来は直書きで根拠も無かった）。値の意味は
[document/05-daemon.md#background-worker-の待ち方](../../document/05-daemon.md#background-worker-の待ち方) の表に載せた。

### 実測（idle daemon、before / after）

fixture workspace（`/tmp` 配下）と専用 `USAGI_HOME` で `daemon serve` を起動し、3 秒の起動安定後 20 秒間 idle に
置いて `ps -o time=` で CPU 時間を測った。before は本 issue の親 commit（47b9e1f2）を同じ harness で測った。

| | CPU 時間 / 20 s | core 使用率 |
|---|---|---|
| before | 0.370 s | **1.848%** |
| after | 0.010 s | **0.050%** |

**idle の CPU は約 37 分の 1 になった。**

fsync の削減について、この process 単位の probe では before / after のどちらでも
`<data-dir>/daemon/user-decisions.json` の作成を観測できなかった（probe が見ていた path が daemon の実際の書き込み先と
一致していたことを確認できていない）。したがって「期限到来が無ければ書かない」性質は process probe ではなく
**unit test で固定した**（core: document も store lock も作られず inode も変わらない / bin: worker を数 tick 回しても
document と `.lock` が存在しない）。
