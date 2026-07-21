---
number: 507
title: fix(daemon): planned restart を active/draining generation rollover に接続する
status: todo
priority: high
labels: [review, v2, daemon, lifecycle, recovery]
dependson: []
related: [209, 221, 275, 350, 492]
parent: 505
created_at: 2026-07-21T21:20:49.574125+00:00
updated_at: 2026-07-21T21:20:49.574125+00:00
---

## 問題・影響

shipping `usagi daemon restart` は [restart usecase](../../crates/daemon/src/usecase/restart.rs) から stop → fresh start を行う。旧 daemon process を終了するため、旧 process が所有する PTY master と live Agent terminal を draining generation として維持できない。新 daemon の起動時 reconcile は unfinished runtime を ownership unknown に落とし、#209 / #350 が完了条件とした planned restart の live reattach を満たさない。

[GenerationCoordinator](../../crates/daemon/src/usecase/generation.rs) と #492 の production ownership authority は単一 daemon 内の fencing 基盤であり、CLI lifecycle、ready handoff、複数 generation endpoint の生存期間には未接続である。

## 対象責務

planned restart を次の順序で行う production lifecycle に置き換える。

1. 現 active generation に rollover intent を durable に予約し、新 generation process / endpoint を起動する。
2. 新 generation が state hydrate、socket bind、health / readiness を完了してから control authority locator を atomic に切り替える。
3. 旧 generation は draining へ移り、新規 session/control operation と spawn を拒否する一方、自 generation が所有する terminal の attach/input/resize/resync/exit/kill を継続する。
4. 新 active は old generation の liveness / exit を購読して durable runtime と capacity を一度だけ更新する。
5. old generation は owned terminal と in-flight terminal command が 0 になった後だけ endpoint / process / registry entry を回収する。

manual `usagi daemon restart` と build/update が要求する planned replacement は同じ lifecycle primitive を使う。新 generation の start / ready / locator commit が失敗した場合は old active を維持し、半端な current locator を公開しない。concurrent restart は operation / generation CAS で一つへ収束させる。

`daemon stop` は rollover と別契約にする。live resource がある通常 stop は #209 に従って拒否し、旧 daemon を hidden draining のまま残して「停止済み」と報告しない。利用者が明示 force-cold / terminate を選んだ場合だけ、terminal termination または interrupted reconcile の結果を完了確認して停止する。その後の `daemon start` は cold resume 経路であり、planned rollover として扱わない。

## 非対象

daemon crash / SIGKILL / OS reboot 後に旧 PTY master fd を回収することは対象外とする。broker / Unix FD handoff は #221、provider-native conversation resume は #503〜#510 の契約に従う。planned restart では provider CLI を再起動せず、同じ PTY / child process を継続する。

## 受入条件

- [ ] shipping `usagi daemon restart` は新 active の readiness 後に authority を handoff し、live terminal を持つ旧 daemon を draining として残す。
- [ ] rollover 中も control operation は active generation だけ、terminal operation は owner generation だけが実行し、late / stale request は effect zero になる。
- [ ] start / hydrate / bind / readiness / locator commit の各 failure で old active が利用可能なままになり、二重 active・二重 spawn・state split-brain を起こさない。
- [ ] restart 後の新規 Agent / Terminal は新 active generation が所有し、旧 resource の終了は new durable state / capacity へ一度だけ反映される。
- [ ] old generation は最後の resource 終了後だけ自動回収され、generation 上限と連続 restart を fail-closed に扱う。
- [ ] manual restart と build/update replacement が同一の tested rollover path を使い、stop → fresh start の bypass を残さない。
- [ ] normal stop は live resource がある限り拒否し、明示 force-cold / terminate は結果を確認してから停止する。後続 start は interrupted / explicit resume 契約へ進み、old draining daemon を隠さない。

## 必須回帰テスト

in-process `GenerationCoordinator::rollover` unit test だけでなく、2 個の実 daemon process、別 Unix socket、実 PTY child を使う integration test を追加する。live PTY あり / なし、readiness failure、locator write failure、ACK loss、concurrent / repeated restart、late old event、generation limit、normal stop refusal、force-cold を含める。

shipping CLI を呼ぶ product E2E では、restart 前後の active / draining PID と generation、Agent child PID、spawn count を記録し、planned restart が provider resume argv を一度も実行しないことを確認する。

## docs / migration

[daemon](../../document/05-daemon.md) と [IPC](../../document/04-ipc.md) に active / draining process、locator commit、failure rollback、resource collection を実装どおり記載する。既存 single-generation record は active 1 件として読み、unknown / corrupt registry から ownership を推測しない。
