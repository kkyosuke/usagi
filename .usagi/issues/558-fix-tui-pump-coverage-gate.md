---
number: 558
title: fix(tui): pump の停止・解放パスを coverage gate が確率で落とす競合から外す
status: done
priority: medium
labels: [review, v2, tui, test, coverage]
dependson: []
related: [551, 554, 557]
created_at: 2026-07-26T12:36:19.358016+00:00
updated_at: 2026-07-26T13:20:25.122568+00:00
---

## 問題・根拠（CI 実測で確定）

`src/runtime/terminal_pump.rs` と `src/runtime/refresh_pump.rs` に**競合に勝たないと踏めない行**があり、
**coverage gate が無関係な PR で確率的に落ちる**。

[#1314](https://github.com/kkyosuke/usagi/pull/1314)（daemon の worker 待ち方の変更。TUI には一切触っていない）で
実際に発生した。

| 実行 | commit | coverage |
|---|---|---|
| 1 回目 | `a75f7e0e` | ❌ 未達 3 行 |
| 2 回目（**コード無変更の再実行**） | `a75f7e0e` | ✅ pass |

同一 commit で結果が変わるので、product の欠陥ではなく test の競合依存である。1 回目に報告された未達行は次の 2 か所。

### 1. `terminal_pump.rs` — 停止の二重確認

`TerminalPump::spawn` の worker loop は停止を 2 か所で見る。

```rust
while !thread_stop.load(Ordering::Acquire) {
    let worked = run_round(&thread_shared.state, &mut fetch);
    let interval = lock(&thread_shared.state).next_interval(worked);
    if thread_stop.load(Ordering::Acquire) {
        break;                     // ← この行が未達になる
    }
    wait_for_next_round(&thread_shared, interval);
}
```

内側の `break` は「round を 1 つ終えてから、次の待ちに入る前の窓」で停止が立ったときだけ踏まれる。
外側の `while` 条件で抜ける経路と競合しており、どちらが先に観測されるかはスケジューリング次第である。

### 2. `refresh_pump.rs` — hang した worker の「まだ解放されていない」分岐

**起票時の記述を訂正する。** 当初この節には「worker が解放を観測する前に test が終わるので `return Ok(1)` が
未達になる」と書いたが、**誤りだった**。解放の待ち合わせは既に実装済みで（`pump.take()` が `Some` を返すまで待つ）、
CI が報告した未達行 L620-621 は `return` ではなく**その逆側**、つまり「まだ解放されていないので sleep して回る」
分岐である。

```rust
loop {
    if *worker_release.lock().unwrap_or_else(PoisonError::into_inner) {
        return Ok(1);
    }
    std::thread::sleep(Duration::from_millis(5));   // ← ここと直前の閉じ括弧が未達
}
```

機構はこうである。test は `entered`（closure の**先頭**で送る）を受け取った直後に、非ブロッキングな
`take` / `wake` を 200 回回してから `release` を立てる。worker が `entered` を送った直後にデスケジュールされると、
再開時には既に `release == true` になっており、**1 回目の判定で `return` してこの分岐を一度も通らない**。
`fetch` に入ったことは「`release == false` を観測した」ことを何ら保証しない。

つまり当初の記述とは向きが逆だったが、**競合依存であるという性質は当たっていた**。

## 影響

- **無関係な PR の coverage gate が確率で落ちる**。原因が自分の変更に無いと判断するには、同一 commit の再実行や
  分岐元 commit との比較が必要で、1 回あたり数分〜十数分の CI と調査時間を消費する。
- 「落ちたら再実行」が習慣化すると、**本物の未達を flake として見逃す**危険がある。#1314 では実際に、
  最初は既知の flake だと誤認しかけた。

## 既存 issue との境界

- [#554](554-perf-tui-frame-io.md)（done）— frame 予算からファイル IO と全画面再構築を外す変更でこの pump 周辺を
  触っている。**frame 予算の設計自体は対象外**で、本 issue は「停止・解放パスを決定的にする」ことだけを扱う。
- [#557](557-perf-daemon-background-worker-idle-wakeup-fsync.md)（done）— daemon 側の worker 待ち方。
  本 issue とは別レイヤ（TUI の pump）であり、#557 はこの flake を**発見した経緯**に過ぎない。原因ではない。
- coverage gate の閾値・exclusion policy は [document/08-coverage.md](../../document/08-coverage.md) と
  [06-conventions.md](../../document/06-conventions.md#coverageoff-例外) が正本。**policy は変えない**。
  `coverage(off)` で黙らせるのは解決にしない（競合依存の行は許可理由 `real_io` /
  `composition` / `generic_monomorphization` のいずれにも該当しない）。

## やること

- `terminal_pump.rs` の worker loop の**停止経路を 1 本にする**。round 後の二重確認をやめ、停止の観測点を
  1 か所（待ちの内側）に寄せる。停止の即応性は落とさない。
- `refresh_pump.rs` の hang test で、worker が「**まだ解放されていない**」ことを観測したことを test 側が待ち合わせる。
  `fetch` への entry 通知だけでは不足である（上記の機構）。

## 設計上の判断が必要な点

- **停止経路を 1 本にしたときの停止レイテンシ**。現在の二重確認は「長い interval の待ちに入る前に停止を拾う」ための
  ものと読める。`wait_for_next_round` が停止で起床する仕組みを持つなら二重確認は不要だが、持たないなら
  1 本化すると停止が interval 分遅れる。**どちらなのかを先に確認する**こと。遅れるなら、待ち側を停止で起こす形
  （`#557` の `ShutdownRequest` と同じく flag + condvar）へ寄せてから二重確認を外す。
- **hang test の待ち合わせをどこに入れるか**。worker が `return Ok(1)` に到達したことを test が観測する必要があるが、
  pump の drop が join するだけでは「到達した」と「hang のまま kill された」を区別できない。
  worker 側から 1 回 send して test が受け取る形が素直だが、**hang を検証する test の趣旨（render thread を
  ブロックしない）を壊さない**こと。
- 同種の競合依存行が他の pump / worker test にも無いか棚卸しする。あれば同じ扱いにするか、別 issue に切る。

## 受入条件

- [ ] `terminal_pump.rs` の worker 停止経路が 1 本になり、停止レイテンシが退行しないことをテストで固定する。
- [ ] `a_hung_fetch_never_blocks_the_render_thread` が「まだ解放されていない」分岐の到達を待ち合わせる。
- [ ] **同一 commit で coverage を複数回実行しても未達が出ない**。少なくとも CI で 2 回連続 green を確認する。
- [ ] `coverage(off)` を追加していない（競合依存は許可理由に該当しないため）。
- [ ] カバレッジ 100% を維持する。`document/` の更新は、停止契約を変えた場合のみ該当節に反映する
      （未実装の契約を先に書かない）。

## 必須回帰テスト・計測

- `cargo test -p usagi --bin usagi`（`runtime::terminal_pump` / `runtime::refresh_pump` の既存テスト）
- **flake の確認**: 変更後に `cargo llvm-cov` を同一 commit で 2 回以上実行し、当該行が毎回踏まれることを確認する。
  1 回の green は本 issue の証拠にならない（1 回目 fail / 2 回目 pass が本 issue の起点である）。
- Rust 差分を含むため、fmt / `cargo check --workspace --all-targets` /
  `cargo clippy --workspace --all-targets -- -D warnings` / `scripts/recommend-tests.sh origin/main` の推奨 test を
  通し、full gate は PR CI で確認する。

## 実装時の決定

### `terminal_pump` は姉妹ファイルの確立済みパターンを移植した

`refresh_pump.rs` の worker loop には**既に停止経路 1 本化が入っており**、その理由がコメントで明文化されていた
（`Drop` が `stop` と同時に `woken` を立てて notify するので、待ちは停止後ただちに返る。二重確認は promptness を
増やさず、`fetch` の中で停止が landed した競合でしか踏めない）。`terminal_pump.rs` だけがこの修正から取り残されて
いたため、同じ形とほぼ同じコメントを移植した。

停止レイテンシが落ちないことは実装前に確認した。`stop.store(true)` は両ファイルで `Drop` の 1 か所だけであり、
そこは必ず `woken = true` と `notify_all` を伴う。したがって待ち側は condvar で必ず起き、round 後の二重確認は
不要である。この性質は
`dropping_the_pump_stops_the_worker_without_waiting_out_the_cadence` で固定した（未登録レーンの 250 ms を
待たずに drop が返ること）。

### 未達行そのものを消した

`terminal_pump` 側は「テストで踏めるようにする」のではなく、**その行を無くした**。競合でしか踏めない行が存在しない
なら未達にもなり得ない。`coverage(off)` は使っていない（競合依存は許可理由に該当しない）。

### ローカルでは再現しない

修正前のコードをローカルで計測すると両ファイルとも 100%（`refresh_pump` 405/405、`terminal_pump` 280/280）で、
未達は CI（Linux・instrumented・並行）でしか出なかった。したがって**ローカルの 1 回 green は本 issue の証拠にならない**。
CI で 2 回連続 green を確認する受入条件はこの理由で置いている。
