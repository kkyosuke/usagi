---
number: 559
title: feat(daemon): serve を durable generation registry の active generation として登録する
status: done
priority: high
labels: [review, v2, daemon, lifecycle, recovery]
dependson: []
related: [209, 221, 275, 350, 492, 507, 508, 515, 516, 518, 528, 550]
parent: 505
created_at: 2026-07-26T13:20:50.130410+00:00
updated_at: 2026-07-26T14:49:11.322179+00:00
---

## 問題・影響

[#507](./507-fix-daemon-planned-replacement-operation-live-runtime-cold.md) は shipping の planned replacement を
1 本の durable operation（`usagi-daemon` の `usecase::replacement`）へ集約し、live runtime を壊す cold transition を
既定で拒否するところまでを閉じた。しかし **seamless rollover 自体はまだ production から起動できない**。

その最初の理由は、**production に generation registry が一切存在しない**ことだった。
[#516](./516-refactor-daemon-cross-process-generation-registry-standby-handoff-authority.md) の
registry / standby / handoff / admission は実装済みだが、`serve` はそれを駆動せず、
`generations.json` は fixture の中にしか現れない。そのため:

- `replacement::seamless_refusal` は build を問わず常に `no generation registry` を返し、
  「何が足りないのか」を名前で示せない（refusal が観測ではなく定数になっている）。
- client 側の `TrustedGenerationDirectory`（[#508](./508-fix-tui-ipc-draining-generation-inventory-terminalref-owner-routing.md)）は
  registry が無いので常に locator へ退化し、owner routing を production で検証できない。
- authority を渡す先を registry から名指す handoff protocol に、**渡す元**すら登録されていない。

## 対象責務

`serve` を durable generation registry の参加者にする。これが seamless rollover の残り 3 配線すべての前提である。

1. `serve` の endpoint 公開を **bind** と **claim** の 2 段に分ける。bind は endpoint を *応答する* 状態にし
   （`SecureUnixListener::bind_private`）、claim は endpoint を *発見可能にする*。この順序により、
   registry entry が「誰も accept していない endpoint」を名指すことも、published locator が
   「registry の知らない generation」を名指すこともない。
2. `usecase::authority::activation` を追加する。claim は (a) 前 incarnation が残した registry / locator を
   `authority::rollover::recover` で reconcile し、(b) 自分の generation を単一の active として registry へ
   compare-and-swap で登録し、(c) locator を publish する。write 順序は handoff protocol の
   `from = None` 版であり、crash boundary の意味は同じ表で読める。
3. registry への登録は handoff ではない。移譲元も移譲先も無いので standby readiness を経由せず、
   1 回の registry CAS で「exactly one active + current がそれを名指す」不変条件を確立する。
   retained generation が 1 つでもある registry は handoff protocol の領分なので `authority_retained` で拒否する。
4. 前 incarnation の retired entry は同じ CAS で捨てる。retired generation は client から見て既に
   addressable でないため、record を残すことは何も述べず、restart 1 回ごとに document を 1 entry 太らせるだけである。
5. 正常終了時は locator → registry entry の順に authority を返却する。この順序なら、
   registry の知らない `current` が publish されている瞬間が存在しない。
6. artifact identity が unknown な build も serve できる。`verified_build` は「自分の hello が
   この artifact を証明した」という意味なので、比較不能な identity は unknown のまま記録し、
   rollover successor にはならない。

## 非対象

standby lifecycle・owner shard への移行・client routing・seamless rollover の有効化。
これらは分割した後続 issue が担う（下記）。

## 受入条件

- [x] shipping daemon の起動で `generations.json` が現れ、active 1 件が `current.json` と同じ generation / endpoint を名指す。
- [x] 記録される process identity は `daemon.json` と同じ start identity token であり、後続 start が exact 比較で生存を判定できる。
- [x] `daemon stop` は authority を返却し、`current` が null、entry が `retired` になる。
- [x] 繰り返し restart しても retained generation は常に 1 件で、document は太らない。
- [x] locator publish の前に crash した場合、次の start は authority を証明できないので fail closed（entry を retire）に収束し、旧 generation の record は残らない。
- [x] 生存している active generation を、別 process の claim が displace しない（`authority_retained` の typed refusal）。
- [x] bind していない状態の claim、canonical でない generation 名の claim は registry を書かずに拒否する。
- [x] endpoint retirement に失敗した場合は authority を返却せず、record を completion fence として残す。
- [x] `replacement::seamless_refusal` は production で `no verified standby` を返す（`no generation registry` は「daemon が一度も起動していない」状態だけを意味するようになる）。

## 残件

元の #559 の 5 責務のうち、本 issue が閉じたのは「registry 参加」である。残りは前提順に分割した。

| issue | 内容 |
|---|---|
| [#563](./563-feat-daemon-serve-role-aware-standby-process-registry.md) | `serve` を role-aware にし、standby process を registry へ登録する |
| [#564](./564-refactor-daemon-production-durable-runtime-state-owner-shard-global-allocator.md) | production の durable runtime state を owner shard / global allocator へ移す |
| [#565](./565-fix-tui-tui-client-ownerrouter.md) | 合成ルートと TUI の client を `OwnerRouter` に載せる |
| [#566](./566-feat-daemon-replacement-seamless-rollover.md) | `replacement` を seamless rollover へ接続して有効化する（元の受入条件と product E2E を引き継ぐ） |

この 4 つが揃うまで seamless rollover は disabled のままであり、shipping の replacement は
old active/current と live PTY を維持した typed refusal か、明示的な cold transition である。
順序が重要な理由は #565 に書いてある: client は `owner-generation-routing.v1` を既に advertise しているが
実装していないため、rollover を先に有効化すると旧 PTY が到達不能になる。
