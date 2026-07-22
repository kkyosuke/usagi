---
number: 519
title: feat(ipc): terminal input ACK lossをcross-connection replayへ収束させる
status: todo
priority: high
labels: [review, v2, core, daemon, tui, terminal, ipc, idempotency]
dependson: [517, 523]
related: [215, 216, 463, 475, 508, 523]
created_at: 2026-07-22T11:37:48.472334+00:00
updated_at: 2026-07-22T11:56:37.187599+00:00
---

## 問題・影響

#517 は受信できた `InputAck` を正しく投影し、ACK loss を effect unknown として blind retry しない。しかし shipping IPC は reconnect 後に同じ terminal input operation の outcome を照会・再取得できない。

`IpcClient` は connection ごとに request sequence を 0 へ戻し、server は `ClientHello` のproducer identityをterminal ledgerに使わずrandom `ClientId`をconnectionごとに発行する。さらにregistryはconnection-owned subscriptionを検証してからcached inputを検索するため、ACK loss後の新connectionでは既存outcomeに到達できない。

加えてunknownな先行inputが未収束のまま同terminalの後続inputをfresh connectionへ送ると、先行処理との順序逆転、sequence gap、または利用者が意図しないcommand連結が起き得る。effect unknownは表示だけでなくper-terminal ordering fenceでなければならない。

## 既存issue / #517 / #523との境界

#215/#216 はproducer-issued identity、response loss、bounded cache、timeout/reconnectの契約を定義済みだがshipping wiringが無い。#517 はACK body decode・同一attachment内sequence整合・fail-closed UXだけを実装する。本issueはconnectionを越えるinput operation identity、ordered convergence、outcome replayを所有し、#517完了に依存する。partial write計測自体は#475を再実装しない。

#523はshared socketのconnection epoch changeを検出し、全paneをfresh subscriptionへ再確立する。#523のfresh connection/attachmentは**epoch-local `input_seq` ledgerを0へreset**するが、未収束のcross-connection input operationやordering fenceを消去しない。本issueはそのdurable operation/client-incarnation ledgerを所有し、subscription epoch recoveryを重複実装しない。

## identity / ordering契約

- `input_seq` はconnection epoch + fresh subscriptionに局所な順序番号で、cross-connection operation identityとして使わない。
- producerのstable `InputOperationId` とsemantic digestはauthenticated client incarnation + full `TerminalRef`にscopeされ、request/reconnect/reattachを越えて同じlogical inputを識別する。
- client incarnation、connection epoch、subscription、epoch-local `input_seq`、cross-connection `InputOperationId` を別identityとしてwire/store/docsに定義する。
- per-terminal producer queueは高々1件のunknown先頭operationをordering fenceにする。そのfinal query/replayがWritten / Failed / Ambiguousへ収束するまで後続inputをPTYへ送らず、bounded queueまたはtyped backpressureにする。
- expiry/ledger lossはtyped unknownのままでblind resendしない。後続送信を再開するには明示的なuser abandonment/recovery policyが必要で、自動的にfenceを越えない。

## 対象責務

- terminal inputごとにproducerがstable operation identityを発行し、request retry / reconnect / reattachでも同じidentityとsemantic digestを維持する。
- authenticated client incarnationとinput operationをdaemonのbounded ledger / response cacheへ配線し、old subscriptionの喪失とoutcome identityを分離する。
- ACK loss後は同じbytesをPTYへ再送せず、Written / Failed / Ambiguous（applied prefixを含む）/ Cached finalを照会・replayする。
- different bytes / target / operation / client scopeへのidentity再利用はconflict。expired outcomeはtyped unknownで、推測resendしない。
- ledger、ordered queue、client identity、subscription、input sequenceのlifetimeとaggregate boundを定義する。

## 受入条件

- [ ] response送信後にsocketを切断しても、reconnect後の同じoperationはPTY write 1回、同じACK finalへ収束する。
- [ ] unknown先行operationのfinal収束前にenqueueした後続inputはPTYへ到達せず、収束後に元の順序で送られる。
- [ ] Written / Failed / Ambiguousの全outcomeがcross-connectionで同じ値を返し、Cached非成功を成功へ変換しない。
- [ ] #523のfresh subscriptionはepoch-local `input_seq`をresetする一方、old operation outcome/fenceをstable operation IDで安全に照合する。
- [ ] conflict / expiry / unknown clientはfail closedで、blind resend・順序逆転・別terminalへの適用がない。
- [ ] cache/ledger/ordered queueはterminal数・client数・byte数・ageでboundedで、超過はtyped backpressureになる。

## 必須回帰テスト

ACK直前/途中/直後切断、Written/Failed/partial Ambiguous、unknown中の複数後続input、same/different payload、cache expiry、#523 epoch change/fresh subscription、daemon/TUI reconnect、複数terminal/clientをdeterministic fake transportと実Unix socketで検証する。PTY writer call countと順序、epoch-local sequence reset、stable operation identityをassertする。

## docs / migration

`document/04-ipc.md` をcross-connection input identity・ordered replay・expiryのSSoTとして更新し、`input_seq` / client incarnation / connection epoch / operation ledgerのlifetime表を置く。wire/schema migrationと旧clientのfail-closed互換を定義する。
