---
number: 209
title: feat(daemon): live terminal を保つ generation rollover と orphan safety を実装する
status: todo
priority: high
labels: [daemon, lifecycle, safety, ipc]
dependson: [216, 218, 219]
related: [159, 205, 206, 207, 208]
parent: 213
created_at: 2026-07-11T12:13:49.117451+00:00
updated_at: 2026-07-12T12:09:54.278892+00:00
---

## 目的

v2 の `DaemonGeneration` ownership を planned stop/restart と crash recovery に配線し、live terminal を持つ旧 daemon を強制停止せず、新規 control operation と terminal を互換な active generation へ移す。設計は [lifecycle/restart](../../document/proposals/05-daemon-lifecycle.md#daemon-lifecyclerestartcrash) を正本とする。

## active／draining rollover

- current locator と generation recordを永続化し、active generationだけがsession state、queue/autostart、新規spawnの実行権威を持つ。
- restartは新generationをreadyにしてcontrol authorityをhandoffし、旧generationをdrainingへ移す。旧generationは所有terminalのattach/input/resize/scrollback/killだけを継続する。
- `TerminalRef` は `DaemonGeneration` を保持し、trusted generation registry が endpoint を解決する。clientはterminal commandを所有generationへ、session/control commandをcurrent activeへ送る。
- active判定はhandshake時だけでなくeffect実行直前とcommit直前にも再検証する。running external IOを持つnonterminal operationがあればrolloverは`busy`。effect前のaccepted/queued intentだけ、old worker停止後にnew activeがowner generation/execution attemptをCASして再開する。
- draining generationはworkspace/session/control stateを書かず、自generationのterminal registry/output journal/input dedupe/kill resultだけを完了する。
- new activeはdraining terminalのExited/liveness streamを購読し、切断時はTerminalList/Reconcileしてruntime reducerとconcurrency slotへ一度だけ反映する。
- live terminal/operationがある通常stopは拒否する。明示drainまたはterminateを別操作とし、terminateはcompleted kill ACKまで待つ。
- 同時generation数は既定2に制限し、さらにrolloverする場合はdrain完了か明示teardownを要求する。

## crash／orphan

- process identityはPIDだけでなく起動identity／process groupと照合し、PID reuse時はsignalを送らない。
- daemon crash後のPTY master fdは復元不能。生存childは`orphan_running`／`identity_unknown`として記録し、attach/writeを拒否する。
- ownership unknownの同一session Agentがある間はreplacement autostart／spawnを止め、verified terminate／gone確認／明示acknowledgeへreconcileする。
- registry不在・破損・ACK timeoutでTerminalRefやstart claimを先に消さない。

## 受け入れ条件

- TUI/clientを切断してもterminalは継続し、planned restart後は旧paneへ正しいgenerationで再attachできる。
- stale client／late worker／別generation requestがstate変更、PTY write、二重spawn、誤killを起こさない。
- terminal 0のdraining generationは自動回収され、使用中generationは最後のresource終了後だけ停止する。
- crash後はscreen継続を偽らず、orphan有無をsnapshot/errorへ明示して安全にreconcileできる。
- rollover、ACK loss、registry corruption、PID reuse、generation limitをfake process／socket／PTY E2Eで検証する。

## スコープ外

PTY brokerまたはUnix FD handoffでmaster fdをdaemon外へ保持する完全なcrash継続は別の将来issueとする。
