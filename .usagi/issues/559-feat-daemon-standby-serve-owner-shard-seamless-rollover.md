---
number: 559
title: feat(daemon): standby serve と owner shard を配線して seamless rollover を有効化する
status: done
priority: high
labels: [review, v2, daemon, lifecycle, recovery]
dependson: [560, 561, 562, 572, 573, 574]
related: [209, 221, 275, 350, 492, 507, 508, 515, 516, 518, 528, 550]
parent: 505
created_at: 2026-07-26T13:20:50.130410+00:00
updated_at: 2026-07-28T23:52:54.509415+00:00
---

## 問題・影響

[#507](./507-fix-daemon-planned-replacement-operation-live-runtime-cold.md) は shipping の planned replacement を
1 本の durable operation（`usagi-daemon` の `usecase::replacement`）へ集約し、live runtime を壊す cold transition を
既定で拒否するところまでを閉じた。しかし **seamless rollover 自体はまだ production から起動できない**。

理由は production 配線が欠けていることであり、authority はいずれも pure な実装として存在する。

| 欠けていた配線 | 実装済みの authority | 状態 |
|---|---|---|
| standby process を起動して registry へ登録する `serve` の lifecycle | [#516](./516-refactor-daemon-cross-process-generation-registry-standby-handoff-authority.md) の registry / standby / handoff / admission | [#561](./561-refactor-daemon-serve-role-aware-standby-process.md) で配線済み |
| owner generation ごとの runtime shard と global allocator | [#518](./518-refactor-daemon-owner-generation-runtime-shard-global-resource-allocator.md) の shard / allocator / exit handoff | [#562](./562-refactor-daemon-durable-runtime-state-owner-shard-global-allocator.md) で配線済み |
| client が `TerminalRef.daemon_generation` で owner endpoint を選ぶ経路 | [#508](./508-fix-tui-ipc-draining-generation-inventory-terminalref-owner-routing.md) の `usagi_core::usecase::owner_routing` | [#560](./560-feat-tui-client-ownerrouter-owner-generation-routing.md) で配線済み |
| active generation の per-request admission fence と routing ledger | [#516](./516-refactor-daemon-cross-process-generation-registry-standby-handoff-authority.md) の `admission` / `routing` / `workers` | 本 issue の PR で配線済み |
| 検証済み standby を active へ昇格させる handoff の起動 | [#516](./516-refactor-daemon-cross-process-generation-registry-standby-handoff-authority.md) の `rollover::execute_gated_rollover` | [#572](./572-feat-daemon-rollover-trigger-ipc-verb-old-active-gated-handoff.md) |
| draining generation の自動回収 | [#516](./516-refactor-daemon-cross-process-generation-registry-standby-handoff-authority.md) の `rollover::collect_retired` | [#573](./573-feat-daemon-draining-generation-claim-0.md) |

この配線が揃うまで seamless rollover を有効化すると、旧 PTY が生存しても到達不能・uncollectable になる。
capability は wire 上すでに双方向で advertise されている（server / client とも `owner-generation-routing.v1`）ため、
残るのは配線だけである。

## 対象責務

1. `serve` を role-aware にする。→ [#561](./561-refactor-daemon-serve-role-aware-standby-process.md)（done）
2. production の durable runtime state を owner shard / global allocator へ移行する。
   → [#562](./562-refactor-daemon-durable-runtime-state-owner-shard-global-allocator.md)（done）
3. 合成ルートと TUI の client を `OwnerRouter` に載せる。
   → [#560](./560-feat-tui-client-ownerrouter-owner-generation-routing.md)（done）
4. **active generation を admission fence と routing ledger に載せる**（本 issue の PR）。
   両 serving role が 1 つの request 分類を読み、request ごとに role・revision・resource owner から
   authority を決め直す。client worker は shutdown 半分とともに保持し、finished だけを回収する。
   これは `execute_gated_rollover` が待つ barrier と `admit_rollover` が読む ledger の**両方**の前提である。
5. `replacement` の preflight を `authority::routing::admit_rollover` と `authority::standby` へ接続し、
   `SeamlessRollover` を `ReplacementPlan` の第 3 の結果として追加する。`execute_gated_rollover` を
   shipping lifecycle から駆動する。
   → [#572](./572-feat-daemon-rollover-trigger-ipc-verb-old-active-gated-handoff.md)
6. old generation は owned resource・lease・outbox・capacity claim がすべて 0 になった後だけ
   endpoint / process / registry entry を回収する。generation 上限と連続 restart は fail closed に扱う。
   → [#573](./573-feat-daemon-draining-generation-claim-0.md)

## 非対象

daemon crash / SIGKILL / OS reboot 後に旧 PTY master fd を回収することは対象外とする。broker / Unix FD handoff は
#221、provider-native conversation resume は #503〜#510 の契約に従う。planned restart では provider CLI を
再起動せず、同じ PTY / child process を旧 draining owner が継続する。

## 受入条件

本 issue 自身が閉じるのは 4 の配線である。残りは子 issue が持つ。

- [x] serving role（`active` / `standby`）はいずれも per-request admission fence を通して全 connection を
      serve し、wire request の分類は 1 か所にある。role ごとの差は「その generation が runtime を所有するか」
      という role 自身の表明だけである。
- [x] active generation が 1 つだけの build では観測できる挙動が変わらない（active role では両 lease class が
      open で、従来 dispatch されていた request はすべて dispatch される）。
- [x] role が `draining` へ移れば、**active として admit された既存 connection 上でも**次の request から
      control と spawn が effect zero で拒否され、所有する terminal の IO は serve され続ける。
      `retired` は terminal IO も含めて何も admit しない。
- [x] active generation は connection ごとに `ClientHello` の routing 回答を記録し、connection が終われば忘れる。
      これが `admit_rollover` の `client_routing_unsupported` 判定の入力である。
- [x] client worker は unblock 用の複製 descriptor とともに保持され、複製できなかった connection は保持しない。
      長命な generation が歴史上の全 connection を保持しないよう finished だけを join して回収する。
- [ ] shipping `usagi daemon restart` は new active の readiness 後に authority を handoff し、live terminal を持つ old daemon を draining として残す。provider resume argv は一度も実行されない。 → [#572](./572-feat-daemon-rollover-trigger-ipc-verb-old-active-gated-handoff.md)
- [ ] handoff 後も TUI close/reopen、client reconnect、active locator 切替をまたいで old `TerminalRef` は owner endpoint へ到達でき、新規 control/launch だけが active へ送られる。 → [#572](./572-feat-daemon-rollover-trigger-ipc-verb-old-active-gated-handoff.md) / [#574](./574-test-daemon-seamless-rollover-product-e2e-2-daemon-process-pty.md)
- [ ] routing capability / registry revision を確認できない client、旧 build、partial deployment では rollover を開始せず、old active/current と live PTY を維持した typed refusal になる。 → [#572](./572-feat-daemon-rollover-trigger-ipc-verb-old-active-gated-handoff.md)
- [ ] start/hydrate/bind/readiness と authority commit 前の failure では old active を維持する。observable commit 後の registry/locator partial phase は roll-forward / repair または fail closed へ収束し、二重 active・二重 spawn・state split-brain を起こさない。 → [#572](./572-feat-daemon-rollover-trigger-ipc-verb-old-active-gated-handoff.md)
- [ ] rollover 中も control/new spawn は active generation だけ、terminal operation は exact owner generation だけが実行し、late/stale request と event は effect zero になる。 → 判定は本 issue で配線済み。rollover 中の観測は [#574](./574-test-daemon-seamless-rollover-product-e2e-2-daemon-process-pty.md)
- [ ] restart 後の新規 Agent / generic Terminal は new active が所有し、old resource の exit は durable state/capacity へ一度だけ反映される。 → [#573](./573-feat-daemon-draining-generation-claim-0.md)
- [ ] old generation は最後の resource/lease/outbox/capacity claim 終了後だけ自動回収され、generation 上限と連続 restart を fail closed に扱う。 → [#573](./573-feat-daemon-draining-generation-claim-0.md)
- [x] `daemon stop` の live-resource refusal（#507 で実装済み）は変わらず、force-cold は結果を確認してから停止する。

## 必須 product E2E

shipping binary、2 個の実 daemon process、別 Unix socket、実 PTY child を使う。
→ [#574](./574-test-daemon-seamless-rollover-product-e2e-2-daemon-process-pty.md) が持つ。

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
cold transition だけが残る条件を正しく書き直す。legacy state は capability と exact identity を検証できる場合だけ移行する。

本 issue の PR で更新したのは、fence 配線によって現在形が変わった箇所である。

- planned replacement 節の「まだ無いのは standby を起動する lifecycle」（#561 で解消済みだった）を、
  揃った前提と残る前提（handoff の起動）の表へ差し替えた。
- endpoint 撤去順の「shipping `serve` は client worker の JoinHandle を保持しない」を現在形へ直した。
- admission fence 節に、両 serving role が読む request 分類表と、「どの record を名指しているかは
  terminal runtime の判断であり fence の判断ではない」という層の分担、client worker の保持・回収を追記した。

`seamless_refusal` の variant の削除と cold transition 条件の書き直しは、それが解消する時点
（[#572](./572-feat-daemon-rollover-trigger-ipc-verb-old-active-gated-handoff.md)）で行う。現在の
`standby not admitted` は「検証済み standby は居るが admit する lifecycle が無い」という正確な現状の記述である。
