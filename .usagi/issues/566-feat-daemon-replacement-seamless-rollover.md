---
number: 566
title: feat(daemon): replacement を seamless rollover へ接続して有効化する
status: todo
priority: high
labels: [v2, daemon, lifecycle, recovery]
dependson: [563, 564, 565]
related: [507, 559]
parent: 505
created_at: 2026-07-26T14:37:38.058819+00:00
updated_at: 2026-07-26T14:37:38.058819+00:00
---

## 問題・影響

3 つの production 配線（standby serve [#563](./563-feat-daemon-serve-role-aware-standby-process-registry.md)、
owner shard [#564](./564-refactor-daemon-production-durable-runtime-state-owner-shard-global-allocator.md)、
client routing [#565](./565-fix-tui-tui-client-ownerrouter.md)）が揃った後に、
shipping の `usecase::replacement` を seamless rollover へ接続する。これが
[#505](./505-fix-v2-claude-codex-agent-tab-usagi-daemon.md) 系列の最後の一手であり、これが入るまで planned replacement は
old active/current と live PTY を維持した typed refusal か、明示的な cold transition のままである。

## 対象責務

1. `replacement` の preflight を `authority::routing::admit_rollover`・`authority::standby::prepare_standby`・
   `resources::migration::planned_rollover_admission` へ接続する。
2. `ReplacementPlan` に第 3 の結果 `SeamlessRollover` を追加する。live runtime > 0 かつ前提がすべて
   満たされている場合にだけ選ばれ、満たされない場合は既存の typed refusal（理由付き）に落ちる。
3. `authority::rollover::execute_gated_rollover` を shipping lifecycle から駆動する。standby process の
   起動 → readiness → handoff → old process を draining として維持、までを 1 本の durable operation にする。
4. `authority::rollover::collect_retired` を駆動する。old generation は owned resource・lease・outbox・
   capacity claim がすべて 0 になった後だけ endpoint / process / registry entry を回収する。
5. generation 上限と連続 restart を fail closed に扱う。

## 非対象

daemon crash / SIGKILL / OS reboot 後に旧 PTY master fd を回収すること（#221 の broker / FD handoff）。
provider-native conversation resume（#503〜#510）。planned restart では provider CLI を再起動せず、
同じ PTY / child process を旧 draining owner が継続する。

## 受入条件

[#559](./559-feat-daemon-serve-durable-generation-registry-active-generation.md) の元の受入条件をそのまま引き継ぐ。

- [ ] shipping `usagi daemon restart` は new active の readiness 後に authority を handoff し、live terminal を持つ old daemon を draining として残す。provider resume argv は一度も実行されない。
- [ ] handoff 後も TUI close/reopen、client reconnect、active locator 切替をまたいで old `TerminalRef` は owner endpoint へ到達でき、新規 control/launch だけが active へ送られる。
- [ ] routing capability / registry revision を確認できない client、旧 build、partial deployment では rollover を開始せず、old active/current と live PTY を維持した typed refusal になる。
- [ ] start/hydrate/bind/readiness と authority commit 前の failure では old active を維持する。observable commit 後の registry/locator partial phase は roll-forward / repair または fail closed へ収束し、二重 active・二重 spawn・state split-brain を起こさない。
- [ ] rollover 中も control/new spawn は active generation だけ、terminal operation は exact owner generation だけが実行し、late/stale request と event は effect zero になる。
- [ ] restart 後の新規 Agent / generic Terminal は new active が所有し、old resource の exit は durable state/capacity へ一度だけ反映される。
- [ ] old generation は最後の resource/lease/outbox/capacity claim 終了後だけ自動回収され、generation 上限と連続 restart を fail closed に扱う。
- [ ] `daemon stop` の live-resource refusal（#507 で実装済み）は変わらず、force-cold は結果を確認してから停止する。

## 必須 product E2E

shipping binary、2 個の実 daemon process、別 Unix socket、実 PTY child を使う。

- live Agent / generic Terminal あり・なし
- readiness failure、registry / locator 各 write 境界の SIGKILL recovery、observable commit の非 rollback
- routing capability 無し / 旧 client / revision mismatch で handoff effect zero
- persistent old connection、in-flight spawn/control、internal background producer
- TUI close/reopen、active locator 切替、draining endpoint 一時不通後も old owner ref へ再接続
- restart response / ACK loss、concurrent / repeated restart、generation limit
- G1 exit と G2 spawn の同時実行、late/duplicate old event、capacity release

restart 前後の active/draining PID と generation、Agent/generic child PID・OS start identity、spawn count を記録する。

## docs / migration

[daemon](../../document/05-daemon.md) の [planned replacement](../../document/05-daemon.md#planned-replacement) と
[IPC](../../document/04-ipc.md) を実装済みの現在形で更新する。`seamless_refusal` の表から解消した variant を落とし、
cold transition だけが残る条件を正しく書き直す。
