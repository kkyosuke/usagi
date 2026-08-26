---
number: 679
title: fix(daemon): PR refresh の gh child を bounded output / process group で回収する
status: done
priority: medium
labels: [review, v2, daemon, pr, process, resource, resilience]
dependson: []
related: [346, 493, 606, 656, 661]
created_at: 2026-08-13T22:45:35.398551+00:00
updated_at: 2026-08-26T00:00:00+00:00
---

## Finding（P2 process / resource）

`src/runtime/daemon.rs::GhProcess::run` は `gh pr view` に5秒timeoutを持つが、次の境界がない。

- stdoutをchild終了後に `read_to_string` し、byte上限がない。
- childをprocess groupへ分離せず、timeoutはparent `Child::kill` のみ。wrapper/descendantがpipeやprocessを保持するとcleanupを証明できない。
- stdoutをchild終了後までdrainしないため、pipe容量を超える出力ではchildがwriteでblockし、正常結果でもtimeoutへ進む。
- status success後の `read_to_string` / descendant pipe closeには別deadlineがなく、5秒provider timeoutがwall-clock completion boundにならない。

PATH上の`gh`がwrapper、壊れた実装、または巨大JSON/診断を出す場合、PR refresh workerのmemory/FD/thread/process lifetimeがboundedではない。

## 修正方針

- stdout/stderrをconcurrent drainし、streamごとのhard byte capを持つ。
- childをowned process group/sessionに置き、timeout/output overflow/observation failureでTERM → bounded grace → KILL → reap → reader joinする。
- parentが先にexitしてdescendantがpipeを保持する場合もgroup cleanupでjoinをboundedにする。
- raw output、path、credentialをerror/log/IPCへ載せず、`RefreshResult::Failed`へ正規化する。
- coreの`bounded_process` primitiveを拡張/再利用できるならpolicyを共通化し、別実装を増やさない。

## 受入条件

- [ ] stdout/stderrのlimit exactly / +1、invalid UTF-8、nonzero、timeoutをsafe failureへ正規化する。
- [ ] parent exit後にdescendantがpipeを保持してもwall-clock bound内に戻り、process groupが残らない。
- [ ]巨大outputでもretained memoryがhard cap内で、worker/inventory lockを保持しない。
- [ ] normal `gh pr view --json title,state` の既存publish/backoff契約を維持する。

## 根拠箇所

- `src/runtime/daemon.rs::GhProcess`
- `crates/daemon/src/usecase/pr_inventory.rs::GhProcessPort`
- `document/05-daemon.md#pr-refresh-scheduler`

## 2026-08-26 対応

- [x] `gh` を共通の bounded process primitive で起動し、stdout / stderr を並行 drain して
      stream ごとに 256 KiB の hard cap を適用した。
- [x] output overflow を reader から owner へ即時通知し、5秒の provider deadline を待たずに
      owned process group を TERM → 100 ms grace → KILL → reap で回収する。
- [x] exact / +1、invalid UTF-8、nonzero、timeout、parent exit 後に pipe を保持する descendant を
      実プロセステストで固定し、全失敗を raw output を含まない safe error へ正規化した。
- [x] 既存の固定 argv、inventory lock 外実行、publish / backoff 契約を維持した。
