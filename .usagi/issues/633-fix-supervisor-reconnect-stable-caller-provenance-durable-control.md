---
number: 633
title: fix(supervisor): reconnect-stable caller provenance で durable control を維持する
status: done
priority: high
labels: [review, security, daemon, mcp, ipc, supervisor, reliability]
dependson: []
related: [326, 328, 521]
created_at: 2026-08-02T23:13:59.885570+00:00
updated_at: 2026-08-03T02:11:09.552170+00:00
---

## Finding（high / trust boundary）

### 脅威モデルと対象

MCP/IPC peerはresponse loss、deadline、切断、再接続、daemon rolloverを起こし得る。durable operationは同じsemantic callerとoperationへ収束し、foreign callerはfail-closedでなければならない。

`src/runtime/daemon.rs::dispatch_supervisor_tool` はcallerを `ipc-connection:<ConnectionId>` とする。`ConnectionId` はacceptごとに新規発行される一方、`crates/daemon/src/presentation/ipc.rs` にはreconnect用client incarnationが既にある。`crates/daemon/src/usecase/supervisor_runtime.rs::SupervisorRuntime::start` はcallerをidempotency semantic keyとdurable `root_caller_ref` に含める。

### 発生条件・影響・根拠

startがcommitした後にresponseを失い、新socketで同じidempotency keyを再送するとcaller文字列が変わり、`operation id was reused with a different supervisor start` になる。get/list/events/cancel/resolveもdurable ownerと一致せず `OwnershipUnknown` になる。

二重runは作られないが、schedulerが継続するrunを元callerが観測・cancel・escalation resolutionできず、durable controlを永久に失う。

### effect-zero 条件

foreign/forged callerのcontrolはmutation 0で拒否する。一方、daemonが同一と証明したreconnect callerは同じoperation/runへ収束し、既存control authorityを回復する。client-declared ID単独はauthorizationに使わない。

## 修正方針

- per-socket ConnectionIdをdurable authorization identityにしない。
- daemon-validated reconnect-stable provenanceを設け、root/session scope、client incarnation、daemon-issued capabilityを束縛する。
- rollover/daemon restart後に継続する範囲と明示失効条件を正本化する。
- semantic retryとownership checkが同じcaller descriptorを使う。

## 必要な回帰テスト

- start response loss → new socket → same idempotency keyが同じrunを返すproduction IPC test。
- reconnect後のget/list/events/cancel/resolveが正規callerで成功する。
- forged/foreign callerは全effect 0。
- reconnect、rollover、daemon restartのmatrix。
- same socketのusecase直呼びだけでなくdispatcher/handshakeを含める。

## 既存 issue との差分

#328 はsupervisor API導入、#326 はdisconnect後もschedulerを継続する契約、#521 はowner routingの関連境界である。socketを跨ぐdurable caller identityとretry収束は未実装・未テストである。
