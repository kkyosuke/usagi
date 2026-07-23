---
number: 528
title: fix(daemon): build artifact identity で safe rollover trigger を判定する
status: done
priority: high
labels: [review, v2, daemon, ipc, lifecycle, build, release, recovery]
dependson: []
related: [215, 275, 341, 461, 507, 508, 516, 518]
parent: 507
created_at: 2026-07-22T12:04:10.797067+00:00
updated_at: 2026-07-23T10:16:18.568559+00:00
---

## 問題・根拠

shipping bootstrap は client binary と daemon の build artifact が本当に同一かを証明できない。

- composition の `current_build()` と core `IpcClient::connect` が作る client identity は `version = CARGO_PKG_VERSION`、`commit = "unknown"`、`target = ARCH` である。
- daemon `ServerHello` も同じ固定 `commit = "unknown"` を返す。
- `build_status` は version / target が非空なら `BuildIdentity` の完全一致だけを見るため、`commit = "unknown"` 自体は unavailable と判定しない。同じ Cargo version / target で別 source/tree から再buildした binary は同じ identity となる。
- production composition は runtime mode に関係なく `connect_or_start(..., force_restart = false, ...)` を呼ぶ。このため同一と誤認した old daemon を再利用し、build replacement も #507 の planned rollover trigger も発火しない。
- done #275 は development channel の `cargo run` が毎 bootstrap 一度 restart すると定義したが、unit seam の `force_restart = true` は shipping compositionへ未配線である。さらにgeneration rollover導入後にこの契約をそのまま配線すると、同じartifactからのTUI / CLI / MCP invocationごとにdaemon churnを起こし、old generationがdrain中でもgeneration上限へ到達し得る。本issueはこのpost-completion gapを、全channelのexact-artifact reuseと明示force-replacementへ訂正する。

version/protocol compatibility と artifact incarnation は別概念である。protocol negotiation が成功しても、実行中 daemon が current client と同じ executable artifact だとは限らない。

## 対象責務

1. build artifact / incarnation identity を定義する。同一配布 artifact では安定し、同じ Cargo version / target でも executable content または source/tree が異なる再buildでは必ず変わる。compile-time git commit + dirty tree hash、verified binary digest、build nonce 等から、安全に比較できる canonical identityを選び、schema/version、profile/channel、targetを含む。daemonはidentityをprocess startup時に一度だけ確定してimmutableにcacheし、各handshakeでexecutable pathを再hashしない。
2. client と daemon は同じ binary artifactから導出した identityを `ClientHello` / `ServerHello` と diagnosticsへ載せる。shipping pathに literal `"unknown"` を生成する分岐を残さない。
3. Git metadataの無いpackage build、dirty tree、cross compile、installed binary、self-update後を含むidentity source / fallbackを定義する。fallbackもartifactを一意に識別できない場合はunknownとしてfail safeにし、version/target一致だけでsame buildへ昇格しない。
4. release / distributed / development / localの全channelはexact artifact identity一致時に既存daemonを再利用する。同じversion/targetでも別artifactならtyped build mismatchとなり、old daemonをcold stopせず一意なrollover trigger / operationへ収束させる。development rebuildはartifact identityが変わるため自動triggerされるが、同じartifactを使う単なるTUI / CLI / MCP invocationではtriggerしない。
5. exact artifactを意図的に入れ替える操作は通常bootstrapと分離した明示force-replacement task / flagにする。force requestはdurable operation IDでbounded coalescingし、concurrent invocationやACK lossでもtrigger 1件へ収束する。old generationがdraining中、generation上限到達、routing capability不足では新triggerをeffect zeroでbackpressure / refuseし、同じartifactの無条件restart loopを作らない。#275の「development毎bootstrap restart」は本契約でsupersedeする。
6. unknown / malformed / unsupported identity、old daemon capability不足、identity read/verification failureはtyped safe outcomeにする。old daemonとそのPTYを継続させたままseamless triggerをrefuseするか、利用者が明示したcold transitionだけを許可し、blind stop/start、local fallback、二重daemon spawnを行わない。
7. concurrent client、ACK/response loss、reconnect、repeated bootstrapは同じartifact pair / channelのrollover triggerをdurable operation IDで一つへ収束させる。identity mismatchの観測だけでold daemonへ停止signalを送らない。
8. exact artifact identityとexpected replacement identityをstandby readiness / generation registryへ渡すcontractを#516へ提供する。#516はprivate standbyとauthority handoffを担当し、本issueはregistry CAS / locator publishを再実装しない。

## 責務境界・依存順

```text
#514 daemon owner identity ─┐
#515 locator recovery ──────┼─> #516 registry / standby / admission
本 issue (#528) ─────────────┘                 |
                                               v
                               #518 owner shards / allocator
                                               |
                                               v
                               #508 owner-generation client routing
                                               |
                                               v
                               #507 shipping rollover / stop / final E2E
```

- 本 issueはartifact mismatchを正しく検出し、old daemonを生存させたsafe rollover triggerまでを所有する。
- #516はtriggerが指すexpected artifactをprivate standbyで検証し、cross-process active/current authorityをhandoffする。
- #508はactive / draining inventoryとexact `TerminalRef.daemon_generation` routingをshipping enable前に完成させる。
- #507はmanual/build/update triggerを同じshipping rollover operationへ接続し、#508 capabilityをgateしたうえでold PTY継続と最終product E2Eを所有する。
- wire protocol compatibilityは#215、verified self-update/stagingは#461の責務を維持する。
- done #275のchannel分離は維持するが、development毎bootstrap restartは本issueのexact-artifact reuse + explicit force policyで置き換える。

## 非対象

- generation registry、standby endpoint、locator handoff、request admission fence（#516）
- owner-generation runtime shard / allocator / event handoff（#518）
- shipping rollover orchestration、stop contract、live PTY継続の最終E2E（#507）
- update artifactのdownload/signature/atomic install（#461）

## 受入条件

- [ ] shipping client/serverのbuild identityにartifactを識別できるcanonical valueが入り、literal `"unknown"` 同士をsame buildとして再利用しない。
- [ ] 同一version/targetでcontentの異なる2 binaryはdifferent artifactとなり、new clientはold daemonを停止せず一つのtyped rollover triggerを返す。old PID、endpoint、live PTYはtrigger consumerがhandoffを開始するまで継続する。
- [ ] release / distributed / development / localはexact same artifactをchild spawn / rollover triggerなしで再利用する。明示force-replacementだけがsame-artifact triggerを発行し、concurrent TUI / CLI / MCP invocation、ACK loss、draining generation、generation limitの下でもbounded coalescingまたはtyped refusalとなる。
- [ ] identity sourceがunknown / malformed / unsupported、またはold daemonがcapability非対応の場合はversion/target一致へfallbackせず、old daemonを維持したtyped refusalまたは明示cold transitionになる。
- [ ] client hello、server hello、standby expected identity、registry record、post-readiness verificationが同じcanonical artifact identityを照合し、TOCTOUで別binaryをactiveにしない。
- [ ] daemonがadvertiseするartifact identityはprocess startupからexitまでimmutableである。self-update / atomic replaceで同じexecutable path上のbinaryが入れ替わっても、old processはnew artifact digestを報告せず、自分が起動したartifact identityを継続する。
- [ ] concurrent/repeated bootstrap、response/ACK loss、client crash/reconnectはartifact pair / channelごとに同じoperationへ収束し、rollover trigger countは1、daemon/PTYへの停止effectは0である。
- [ ] identityはsecret、absolute build path、user-specific metadataをwire/log/storeへ漏らさず、reproducible releaseとintentional rebuildのpolicyをdocumentationで固定する。

## 必須テスト

- same Cargo version / targetでcontentだけ異なる2つの実binaryを作り、old daemon継続中にnew clientを接続するprocess E2E。identity mismatch、typed trigger、trigger count 1、old daemon PID / endpoint /実PTY継続を確認する
- exact same artifactをrelease / distributed / development / localの各channelで再利用し、通常TUI / CLI / MCP bootstrapがtrigger 0であるE2E。明示force-replacementはconcurrent caller / ACK lossでもtrigger 1となり、draining / generation limitではtyped refusalとなる
- Git metadata無し、dirty tree、installed binary、cross-target fixture、unknown/malformed identity、old capability非対応のfail-safe matrix
- concurrent clients、trigger response/ACK loss、reconnect、同じartifact pairの再送でoperation IDとoutcomeが一つになるbarrier test
- post-start / standby readinessでexpected artifactと実server artifactが不一致の場合、old daemonを維持してeffect zeroで拒否するtest
- daemon起動後にexecutable pathをself-update相当の別binaryへatomic replaceし、old processの複数`ServerHello`がstartup時identityのまま、新processだけがnew identityを報告するimmutable cache E2E
- #516 / #518 / #508 / #507統合時はsafe triggerがprivate standby handoffへ接続され、owner-generation routing capability確認後だけshipping rolloverを開始し、same-version replacement後もold live PTYが到達可能なdraining ownerとして継続するE2E

## docs / gate

[IPC](../../document/04-ipc.md) と [daemon](../../document/05-daemon.md) は、現行shipping identityが `commit = "unknown"` でsame-version rebuildを区別できず、production compositionの`force_restart = false`により全channelがsame tupleを再利用する事実を現在形で記載する。実装時は全channel exact-artifact reuse、explicit force-replacement、immutable startup identity、unknown policyへ更新する。

Rust、build metadata、wire schema、cross-process lifecycle、実binary E2Eに影響するため、fmt/check/clippy、selected/full tests、coverage 100%、Markdown link checkを必須とする。
