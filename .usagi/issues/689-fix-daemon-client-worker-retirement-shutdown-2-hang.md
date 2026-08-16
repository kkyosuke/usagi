---
number: 689
title: fix(daemon): client worker の retirement が shutdown(2) の起こし損ねで無期限に hang する
status: done
priority: high
labels: [v2, daemon, lifecycle, bug]
dependson: []
related: [682, 683]
created_at: 2026-08-16T22:56:07.535668+00:00
updated_at: 2026-08-16T23:36:17.935389+00:00
---

## 問題・影響

`ClientWorkers::retire()` が、blocking read で park している worker を起こせないまま
`JoinHandle::join()` に入り、**無期限に固まる**。macOS（Darwin）で再現する。

これは test だけの話ではない。`retire()` は**production の retirement barrier そのもの**で、

- daemon の accept loop が shutdown 時に呼ぶ（`src/runtime/daemon.rs` の `spawn_ipc_server` 末尾）
- rollover collection が同じ barrier を共有する（"Active shutdown and rollover collection share the same barrier"）

したがって起こし損ねると **`daemon serve` が終了しなくなり、rollover が完了しない**。
`daemon restart` は旧世代が drain しきるのを待つので、そこで止まる。

## 再現

```bash
# 1 test を 400 回。1 回 10 秒を超えたら hang とみなして stack を取る
cargo test -p usagi --bin usagi only_a_collectable_client_worker_is_retained
```

`runtime::daemon::tests::only_a_collectable_client_worker_is_retained` を単独で 400 回回して
**6 回 hang**（1.5%）。通常の実行時間は 1 秒未満なので、10 秒超は明確な hang である。
6 回とも stack は同一だった。

```text
# 親スレッド: shutdown 済みのはずの worker を join して止まっている
_pthread_join (in libsystem_pthread.dylib)
usagi_daemon::usecase::authority::workers::retire_workers
runtime::daemon::tests::only_a_collectable_client_worker_is_retained  daemon.rs:16845

# worker スレッド: peer 側 socket の read で park したまま
runtime::daemon::tests::only_a_collectable_client_worker_is_retained  daemon.rs:16835
__recvfrom (in libsystem_kernel.dylib)
```

full suite（`cargo test --workspace`）でも実際に踏んだ。`usagi` bin の unit test が 23 分以上
進まなくなり、他の gate 実行を巻き込んで止まった。

## 原因

`retire_workers`（`crates/daemon/src/usecase/authority/workers.rs`）は
**`shutdown(2)` が必ず reader を起こすことを前提に、上限なしで join している**。

```rust
for worker in workers {
    if let Err(error) = worker.connection.shutdown() {
        report.shutdown_failures.push(error);
    }
    handles.push(worker.handle);
}
for handle in handles {
    if handle.join().is_err() { report.panicked += 1; }   // ← 上限が無い
    report.joined += 1;
}
```

`AcceptedStream::shutdown` は複製した descriptor に `shutdown(Shutdown::Both)` を呼ぶ。
Darwin の AF_UNIX では、この起こしが**取りこぼされることがある**。`shutdown()` 自体は `Ok(())` を
返すので `shutdown_failures` には何も残らず、原因の痕跡が一切出ないまま join で固まる。

つまり報告の設計にも穴がある。`shutdown_failures` は「`shutdown()` が Err を返した」ことしか
拾えず、「`shutdown()` は成功したが reader が起きなかった」という実際に起きるほうを拾えない。

## 対象責務

`shutdown(2)` の起こしを**唯一の手段にしない**。retirement はまず flag を公開してから socket を shutdown し、
worker は次の frame を待つ read を毎回 bounded な readiness 待ちで囲って、待ちが空振りするたびに flag を確認する。

### なぜ receive timeout ではなく `poll(2)` か

当初は socket の receive timeout（`SO_RCVTIMEO`）で足りると考えたが、**足りなかった**。
実測（同じ socketpair・同じ dup 関係を再現した最小プログラム、各 1000 回）:

| 構成 | park した回数 |
|---|---|
| receive timeout なし（従来） | 3 / 1000 |
| receive timeout あり、shutdown は**peer**側の複製 | 0 / 1000 |
| receive timeout あり、shutdown は**同じ socket**の複製（＝production の形） | 2 / 1000 |
| `poll(2)` で readiness を先に判定（採用案） | **0 / 6000** |

つまり、socket が shutdown 済みの状態では receive timeout も尊重されない。待ちを socket ではなく
`poll(2)` 側に置き、**何も無い socket で `recv` に入らない**ようにして初めて解消する。

なお最初の再現実験が 0/2000 で「再現しない」と出たのは、その実験自身が `set_read_timeout` を
設定していて、偶然この修正の一部を適用していたためである。

### write 側

対象外とした。worker が無期限に park するのは「次の frame を待つ read」であって write ではなく、
write を非 blocking 化すると同じ open file description を共有する reader 側の複製にも `O_NONBLOCK` が
波及して frame 書き込みが壊れる。write の挙動は従来どおりで、回帰は無い。

### bounded join

入れなかった。readiness 待ちが flag を必ず観測するため join は必ず返る。上限を足すと、
「起きなかった worker を abandon するのか」という barrier の安全性契約（全 worker を必ず join する）を
変える判断が要り、しかもその分岐を決定的にテストできない。本 issue の欠陥は待ち方の問題なので、待ち方で閉じる。

## 受入条件

- [x] 上の 400 回ループで hang が 0 回（修正前は 6 回）。
- [x] readiness 待ちが idle policy になっていない（park を観測してから frame を書き、正しく配送されることを test で固定）。
- [x] daemon の shutdown / rollover collection の契約（全 worker を必ず join する）は弱まっていない。
