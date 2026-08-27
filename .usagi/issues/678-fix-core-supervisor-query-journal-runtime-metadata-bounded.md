---
number: 678
title: fix(core): supervisor 履歴を query・journal・runtime metadata 全体で bounded にする
status: in-progress
priority: medium
labels: [review, v2, core, daemon, supervisor, resource, retention, performance]
dependson: []
related: [325, 328, 585]
created_at: 2026-08-13T22:45:35.302573+00:00
updated_at: 2026-08-26T00:00:00+00:00
---

## Finding（P2 resource / performance）

#585 は snapshot replay checkpoint を追加し、通常の `load()` が反映済み journal を再生しないようにした。しかし履歴の公開・保持側には次が残る。

- `SupervisorStore::events(cursor, limit)` は `read_journal(id)` で journal 全件を parse してから filter/take する。limit 1でも costは全履歴に比例する。
- event journal は append-only のまま retention/compactionがない。
- snapshot の `SupervisorRun.applied_events` は event IDを全件保持する。
- `supervisor-scheduler.json` の `starts` / delivered `wakes` は削除されない。
- terminal run自体の snapshot/journal/checkpointにretentionがなく、`SupervisorRuntime::list` / `tick_all` は全runを列挙する。listのpaginationは全件load後である。
- start inputのtask count / TaskId / instruction fieldは1 MiB transport cap以外のdomain limitを持たない。

長寿命daemonではdisk、snapshot rewrite、list/tick、event pageがrun数・event数に比例して増え続ける。

## 対象責務

- `supervisor_events` を cursor位置からstream/seekし、read/parse/response costをpage limitに比例させる。
- terminal run、event history、applied-event idempotency、start/wake reservationにcount/byte/ageのhard capとminimum replay windowを定義する。
- event payloadをcompactする場合もold event IDの再送をfresh eventとして適用しないtombstone/watermark contractを持つ。
- live/nonterminal run、未配送wake、window内start reservationはGCしない。safe候補なしでcap到達時はnew supervisor start/eventをtyped backpressureでeffect zeroに拒否する。
- task countと各durable string fieldにdomain hard limitを置き、transport capをbusiness/resource policyの代用にしない。
- listもpage query前に全terminal runをhydrateしないindex/cursorを持つ。

## 受入条件

- [ ] 100k event journalでlimit 1/100のread bytesがpage/cursorに比例し、全journalに比例しない。
- [ ]大量terminal run後もsnapshot/journal/scheduler stateがhard cap内に収まり、list/tick costが全historyへ線形増加しない。
- [ ] crash境界を跨いだcompaction後もduplicate event/startはeffect zeroまたはtyped expiredで、fresh適用しない。
- [ ] live run / unacked wakeだけでcapを満たす場合はsilent evictionせずbackpressureする。
- [ ] #585 の replay checkpoint と reducer sequence/CAS契約を回帰させない。

## 根拠箇所

- `crates/core/src/infrastructure/store/supervisor.rs`
- `crates/core/src/domain/supervisor.rs::SupervisorRun::applied_events`
- `crates/daemon/src/usecase/supervisor_runtime.rs::RuntimeState`
- `src/runtime/daemon.rs::dispatch_supervisor_tool`

## 2026-08-24 時点の進捗（v3.0.0 リリースレビュー）

- [x] terminal run の retention（`RUN_RETENTION` = 128）。`prune_finished_runs` が
      snapshot / journal / checkpoint をまとめて削除し、live run（`Planning` /
      `Running` / `WaitingForDecision` / `Verifying`）は年齢によらず残す。
      新しい run の `initialize` で best-effort に走らせるため、maintenance tick に
      依存しない。これで `supervisor_list` の cost が「起動した run の総数」ではなく
      「保持している run 数」で頭打ちになる。
- [ ] event journal の runtime metadata 側の bound — **未対応**
- [ ] query 応答の byte 上限 — **未対応**（`events` は既に cursor + limit を持つ）

## 2026-08-26 対応

- [x] sequence→byte offset の derived index を追加し、100k event の journal でも page 1 / 100 の
      journal read・parse量を page size に比例させた。旧 journal は一度だけindexを再構築する。
- [x] journal は4,096件で最新2,048件へ atomic compact し、古い cursor は typed expired にする。
      snapshot の exact applied ID も同じwindowへ縮め、compact 済みIDは固定長 fail-closed tombstoneで
      fresh sequenceとしての再適用を拒否する。
- [x] scheduler start 256件 / wake 512件のhard capを追加した。終了 / 配送済みだけをtombstoneへ移し、
      live / unackedだけで満杯ならcapacity errorでeffect-zeroにする。
- [x] 初期task / dependency数とTask ID / instruction / artifact / idempotency key / policy selectorを
      UTF-8 byte hard limitでwire schemaとdaemon admissionの両方から拘束した。
- [ ] run list のcursor index化と、query全体のserialized byte budgetは別差分として残る。
