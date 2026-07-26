---
number: 555
title: perf(daemon): PR identity 抽出を PTY 出力 hot path から外す
status: in-progress
priority: high
labels: [review, v2, daemon, core, terminal, pullrequest, performance]
dependson: [552]
related: [493, 518, 524, 526, 527, 534]
created_at: 2026-07-25T22:58:44.664415+00:00
updated_at: 2026-07-26T01:04:53.304381+00:00
---

## 問題・根拠（コード調査で確定）

daemon は PTY output chunk **1 個ごとに** PR identity の抽出と durable inventory の読み込みを行い、しかもそれを
**agent / terminal runtime の lock を保持したまま**実行している。したがって **daemon の lock 保持時間が agent の出力
バイト数に比例して伸びる**。

### 経路（すべてこの worktree の HEAD で確認。行番号は起票時点）

1. **PTY reader は 4096 byte 固定読み・1 read = 1 observation**
   `src/runtime/daemon.rs` の agent PTY spawn（`let mut bytes = [0_u8; 4096];` / `bytes[..count].to_vec()`、
   起票時点 920〜926 行）と generic terminal PTY spawn（同 1077〜1083 行）。
   → 1 MB/s 出力する agent は **毎秒 256 observation**（1 MiB ÷ 4096 B）を生む。

2. **observer が runtime lock を保持したまま PR 抽出を呼ぶ**
   `src/runtime/daemon.rs` の `start_agent_observer`（起票時点 1864 行）は
   `let Ok(mut agent) = agent.lock() else { break };` を loop の先頭で取り、その guard が生きている `match` の中で
   `agent.output(...)` に続けて `pr_inventory.lock()` → `observe_committed(...)` を呼ぶ。
   `start_terminal_observer`（同 1959 行）も同じ構造である。
   → **PR 抽出と durable IO の全時間が agent / terminal lock のクリティカルセクションに入る**。この lock は IPC client
   thread（`dispatch_agent` / `dispatch_session` / terminal snapshot）が取るものと同じなので、TUI から見ると
   「daemon が一時的に応答できない」になる（[document/03-tui.md](../../document/03-tui.md) の
   「dispatch 中に agent lock を保持している間」）。

3. **`observe_committed` が chunk ごとに行っている処理**
   `crates/daemon/src/usecase/pr_inventory.rs` の `OutputPrProjector::observe_committed`（起票時点 261 行）:

   | 手順 | 内容 | chunk ごとのコスト |
   |---|---|---|
   | tail の materialize | `tail.iter().copied().collect()` で `VecDeque<u8>`（上限 4096）を 1 byte ずつ `Vec` へ | 最大 4 KiB のコピー |
   | 全走査 | `extract(&combined)`（tail + 新 chunk = 最大 8 KiB） | **前半 4 KiB は前回すでに走査済みの再スキャン** |
   | tail の更新 | `tail.extend(bytes.iter().copied())` → `while tail.len() > 4096 { tail.pop_front(); }` | 1 byte ずつ push / pop |
   | **durable read** | `self.store.load()?` → `PrInventoryStore::load` → `json_file::read` | **ファイル読み込み + JSON parse** |
   | durable write | 変化時のみ `self.store.save(&sessions)?`（`json_file::write_atomic` = fsync + rename + dir fsync） | 新規検出時のみ |

   `store.load()` は変化の有無に関わらず**毎 chunk 実行される**。`PrInventoryPort for PrInventoryStore`
   （`crates/core/src/usecase/pr_inventory.rs`）は `save` 側で `sessions.clone()` も行う。

4. **同一 chunk の 3 重コピー**
   reader の `bytes[..count].to_vec()` → observer の `bytes.clone()`（PR スキャン用の複製）→
   `crates/daemon/src/usecase/runtime.rs` の `append_output` 内 `output.data.clone()`（起票時点 929 行）。
   4 KiB chunk あたり 3 回の heap 割り当てとコピーになる。

### 実測（この worktree の HEAD、`--release`、macOS / Darwin 25.2.0）

production と同じ `OutputPrProjector<PrInventoryStore>` を tempdir 上に組み、20 session × 5 entry
（`pr-inventory.json` = 23,823 byte）を投入したうえで、URL を含まない 4096 byte の典型的な agent 出力を
1000 chunk 流した。比較のため同じ投影を in-memory port で回した。

| 経路 | 1 chunk あたり | 出力 1 MB あたり |
|---|---|---|
| durable store（production の配線） | **193.4 µs** | **49.5 ms** |
| in-memory port（disk read + JSON parse を除く） | 54.4 µs | 13.9 ms |
| `extract` のみ（8 KiB の combined window、URL なし） | 21.1 µs | 5.4 ms |

- **disk read + JSON parse の取り分 = 139.0 µs / chunk（全体の 72%）**。
- 1 MB/s 出力する agent では **毎秒 256 回の disk read + JSON parse が agent lock 保持下で走り**、
  lock の追加保持時間は約 **49 ms/s**（193 µs のスライス × 256）になる。CPU 時間だけでなく blocking file read
  なので、負荷時の tail latency は CPU 見積りより悪化する。
- `extract` は同じ 8 KiB window に対して URL 数に**超線形**である（1 URL: 33.5 µs、8: 121.0 µs、64: 821.3 µs、
  256: 2,936.5 µs）。`extract` は候補ごとに残りバッファを再走査するためで、`gh pr list` の出力のように URL が
  密な chunk では 1 chunk で ms 級に達する。

### 併せて確認した派生欠陥

- **`tails` map が回収されない**: `OutputPrProjector.tails: BTreeMap<TerminalId, VecDeque<u8>>` に entry を追加する
  経路（`observe_committed`）はあるが、削除する経路がどこにも無い。`PtyObservation::Exited` /
  `AgentPtyObservation::Exited` は projector を触らない。**observe された terminal 1 個あたり 4 KiB が daemon の
  process 寿命いっぱい残る**。[#526](526-fix-daemon-terminal-agent-tombstone-retention-aggregate-bound-gc.md) の
  aggregate retention GC は runtime / final tombstone が対象で、この map は含まない。
- **snapshot 経路も毎回 disk を読む**: `dispatch_pr_snapshot` → `OutputPrProjector::snapshot` → `store.load()`。
  TUI が PR snapshot を polling するたびに IPC client thread が inventory lock 内で disk read + JSON parse を行う。
  `store.load()` の cache 化はこの経路にも効く。

## 既存 issue との境界

- [#526](526-fix-daemon-terminal-agent-tombstone-retention-aggregate-bound-gc.md)（done）— terminal / Agent final の
  retention 会計。**retention の budget・eviction・GC は本 issue の対象外**。ただし上記 `tails` map は #526 の
  aggregate bound の管理下に無いので、本 issue が回収を入れる。
- [#493](493-fix-daemon-pr-refreshscheduler-production-worker.md)（done）— PR **refresh のスケジューリング**
  （tick / coalesce / backoff / freshness）。**refresh cadence は本 issue の対象外**であり、
  [document/05-daemon.md](../../document/05-daemon.md) の PR refresh scheduler 契約は変更しない。本 issue の対象は
  「PR identity の *抽出* が PTY 出力 hot path に載っている」ことだけで、別問題である。
- [#534](534-feat-daemon-terminal-grid-authority-revision-2-checkpoint-snapshot.md) /
  [#524](524-fix-terminal-raw-64kib-tail-vt-parser-safe-snapshot.md)（いずれも done）— checkpoint 生成が per-chunk で
  走らないことは既に対策済みである。`crates/daemon/src/usecase/runtime.rs` の `append_output` に
  「Offsets only: journaling an accepted chunk must not capture a screen, or every PTY chunk would pay for a full
  checkpoint」というコメントで固定されている。**この性質を壊さないことを受入条件に含める**。
- [#518](518-refactor-daemon-owner-generation-runtime-shard-global-resource-allocator.md)（todo, high）— lock 粒度と
  cross-process の single writer を扱い、項目 5 で「旧 PTY observation から共有 `pr-inventory.json` を whole-save する
  経路」を shared writer として inventory 化する対象に挙げている。**重なるのは「`pr-inventory.json` の書き手を誰にするか」
  だけ**であり、そちらが正本である。本 issue は **同一 process 内の出力 hot path の corrective optimization** に限定し、
  owner generation shard・global allocator・cross-process fence は導入しない。本 issue が入れる in-memory cache と
  非同期 worker は、#518 の single-writer / generation fence 契約を先取りして壊さない形（後述の設計論点）にする。
- [#527](527-perf-tui-terminal-polling-ui-loop-foreground-cadence.md)（done）— TUI 側の polling cadence。
- TUI / IPC client 側の「描画スレッドの同期 IPC」は本 issue と相互に増幅する（daemon が lock を握っている間に
  TUI の frame loop が同期 IPC で待つ）。そちらは**別の triage session が起票する TUI frame loop 側 issue** が扱う。

## やること

1. **`store.load()` を hot path から外す**。projector が durable snapshot の in-memory authority を持ち、
   `observe_committed` / `snapshot` は cache を読む。disk read は起動時の hydrate と、外部からの書き換えを
   検出する必要がある場合だけにする。
2. **PR 抽出を runtime lock の外へ出す**。出力の取り込み（registry への append）と PR 抽出は本来独立なので、
   observer は lock 内では append だけを行い、抽出は bounded queue 経由の別 worker で行う。
   queue が満杯のときは **drop または coalesce**（最新の tail を残して中間 chunk を捨てる）で bounded に保ち、
   落ちた量を metrics に出す。
3. **重複再スキャンをやめる**。前回すでに走査した領域を再走査せず、chunk 境界を跨ぐ URL のために必要な
   overlap だけを保持する。
4. **同一 chunk の 3 重コピーを解消する**。reader → observer → `append_output` を `Arc<[u8]>` 等の参照共有にする。
5. **`tails` の回収経路を入れる**。terminal exit / reclaim で該当 entry を落とし、加えて entry 数にも上限を持たせる。

## 設計上の判断が必要な点

- **overlap をどれだけ持つか**。現状の 4096 byte は「tail の上限」であって「URL が跨ぐのに必要な量」ではない。
  必要量の根拠を決めて明記する。候補は (a) canonical PR URL の最大長から導く定数
  （`https://github.com/<owner>/<repo>/pull/<number>` の owner / repo / number の上限）、(b) 行境界基準
  （直近の未完結行だけを持ち、改行で確定した領域は二度と走査しない）。(b) は overlap が入力依存になるため、
  改行を含まない長大出力に対する hard cap が別途必要になる。
  **`extract` の終端規則（空白・制御文字・`'"<>` で終端）と整合させる**こと。
- **bounded queue が落ちたときの意味論**。PR 検出は best-effort でよいのか、それとも drop を「後で必ず追いつく」
  形にするのか。coalesce するなら「捨てた中間 chunk に含まれていた URL は検出されない」ことを受け入れるのか、
  overlap を広げて救うのかを決める。drop は必ず metrics に出す（無言の欠落を作らない）。
- **cache と durable snapshot の一貫性**。projector が in-memory authority を持つと、
  `pr-inventory.json` を書く別の writer（user の pin / dismiss、refresh worker、そして
  [#518](518-refactor-daemon-owner-generation-runtime-shard-global-resource-allocator.md) が扱う draining generation）
  との関係を決める必要がある。**同一 process 内の全 writer が同じ cache を経由する**単一 writer にできれば #518 の
  contract と衝突しない。cross-process の writer を許すなら cache の invalidation 条件を明示する。
  ここを曖昧にすると lost update になるため、実装前に決める。
- **抽出 worker の shutdown**。新しい worker を足すので、shutdown 応答は sleep スピンではなく
  channel disconnect / Condvar で行う（別 issue の background worker idle cost と方針を揃える）。
- **`extract` の超線形性を本 issue で直すか**。URL が密な chunk では抽出自体が ms 級になる。worker へ出せば
  lock 保持時間からは外れるので、計算量の改善は別途でもよい。どちらにするかを決め、別にするなら根拠を書く。

## 受入条件

- [ ] **出力 chunk あたりの disk read 回数が 0（cache hit 時）**である。テストで read 回数を数えて固定する。
- [ ] **agent / terminal lock の保持時間が出力バイト数に比例しない**。lock 内に PR 抽出・durable IO が無いことを
      テストで固定する（出力量を 2 桁変えても lock 内の作業量が変わらない）。
- [ ] **PR identity の検出漏れ・重複が発生しない**。chunk 境界を跨ぐ URL、1 chunk に複数 URL、同じ URL の再出現、
      複数 session からの同一 URL を含む。[#552](552-fix-core-extract-http-https-pr-url.md) で修正した
      `http://` / `https://` 混在の検出も引き続き通る。
- [ ] **`append_output` が checkpoint を作らない性質を維持する**（`crates/daemon/src/usecase/runtime.rs` の
      offsets-only journaling）。per-chunk で screen checkpoint を取らないことをテストで固定する。
- [ ] bounded queue の **drop / coalesce が metrics に出る**。既存の terminal pipeline counters
      （`crates/daemon/src/usecase/terminal.rs` の `output_pipeline_counters` / `MetricsSnapshot` の
      `terminal_backpressured_bytes`）と同じ形（process-local な byte / count の atomic counter、出力内容や
      identity を含まない）で追加する。
- [ ] `tails` 相当の per-terminal 状態が terminal exit / reclaim で回収され、entry 数に上限がある。
- [ ] user の pin / dismiss と refresh worker の publish が cache 経由でも lost update しない。
- [ ] 4 KiB chunk あたりの heap コピー回数が 3 → 1 になる。
- [ ] カバレッジ 100% を維持する。[document/05-daemon.md](../../document/05-daemon.md) の PR 投影に関する記述と
      [document/02-architecture.md](../../document/02-architecture.md) の「検出は増分で行い」の記述を実装に合わせて
      更新する（新しい metrics counter を出すなら [document/05-daemon.md](../../document/05-daemon.md) の
      metrics 節にも反映する）。**未実装の契約を先に書かない**。

## 必須回帰テスト・計測

- `cargo test -p usagi-daemon`（`usecase::pr_inventory` の投影 test。chunk 境界・複数 session・pin/dismiss 保持）
- `cargo test -p usagi-core`（`domain::pr_inventory` / `infrastructure::store::pr_inventory`）
- `cargo test -p usagi --bin usagi`（`src/runtime/daemon.rs` の observer / worker 配線）
- **read 回数の計測 test**: `PrInventoryPort` の load / save 呼び出し回数を数える fake を使い、N chunk 投入後の
  load 回数が chunk 数に依存しないことを assert する。
- **lock 保持の回帰 test**: observer が runtime lock 内で durable port を触らないことを、
  port 呼び出し時に lock を取り直せるか（または呼び出し自体が起きないか）で固定する。
- **before/after の実測**: 本 issue の起票時と同じ条件（20 session × 5 entry、4096 byte chunk × 1000、`--release`）で
  1 chunk あたりの µs と出力 1 MB あたりの ms を測り直し、PR 本文に before / after を載せる。
  URL が密な chunk（8 / 64 / 256 URL）も同じ表に載せる。
- 実 PTY の E2E（`crates/daemon/tests/agent_real_pty.rs` 系）が退行しないこと。重い実 PTY E2E は
  [06-conventions.md](../../document/06-conventions.md#重い-e2e-の直列化) の直列化規約に従う。
- Rust 差分を含むため、fmt / `cargo check --workspace --all-targets` / `cargo clippy --workspace --all-targets -- -D warnings` /
  `scripts/recommend-tests.sh origin/main` の推奨 test を通し、full gate は PR CI で確認する。
