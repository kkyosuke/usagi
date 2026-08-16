---
number: 689
title: fix(daemon): client worker の retirement が shutdown(2) の起こし損ねで無期限に hang する
status: todo
priority: high
labels: [v2, daemon, lifecycle, bug]
dependson: []
related: [682, 683]
created_at: 2026-08-16T22:56:07.535668+00:00
updated_at: 2026-08-16T22:56:07.535668+00:00
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

`shutdown(2)` の起こしを**唯一の手段にしない**。

- worker が park する read に timeout を持たせ（`SO_RCVTIMEO` 相当）、定期的に起きて
  retirement 要求を確認して自分で抜ける。起こしの取りこぼしが「遅い」に縮退し、hang にならない。
- `retire_workers` の join に上限を持たせ、超えたら worker を名指しして報告する。
  barrier が黙って止まる状態を残さない（`RetireReport` に「起こしたが返ってこなかった」を追加する）。

いずれも「観測できるまで駆動し、上限で失敗させる」形であり、固定 sleep や deadline 延長ではない。

## 非対象

- この test の書き換え。test は product の契約（"Retirement shuts the retained half down, which is
  what lets the join return. A test that hung here would be reporting a real defect."）を正しく
  主張しており、**hang しているのは defect が実在するからである**。test 側を緩めない。

## 未確認

- Linux（CI の ubuntu-latest）で同じ取りこぼしが起きるかは未確認。手元の再現はすべて macOS。
  CI で `full-test` が hang した実績はまだ観測していない。ただし production の対象 platform は
  macOS を含むため、platform 依存であっても直す価値がある。

## 受入条件

- [ ] 上の 400 回ループで hang が 0 回。
- [ ] `retire()` が、起こしに失敗した worker を上限つきで検出し、報告に残す。
- [ ] daemon の shutdown / rollover collection の契約（全 worker を必ず join する）は弱まっていない。
