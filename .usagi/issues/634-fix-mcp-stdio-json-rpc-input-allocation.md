---
number: 634
title: fix(mcp): stdio JSON-RPC input を allocation 前に上限制御する
status: done
priority: medium
labels: [review, security, mcp, cli, protocol, reliability]
dependson: []
related: [482, 604]
created_at: 2026-08-02T23:13:59.982202+00:00
updated_at: 2026-08-03T02:05:30.299400+00:00
---

## Finding（medium / availability）

### 脅威モデルと対象

MCP host/provider stdinはmalformed、巨大、invalid UTF-8、改行なしstreamを送り得る。`crates/cli/src/mcp/serve.rs::serve_with_client_and_features` は入力byte admissionの責務を持つ。

### 発生条件・影響・根拠

serverは空の `Vec` に `BufRead::read_until(b'\n', &mut buf)` し、行全体を確保した後で初めてUTF-8変換とJSON-RPC parseを行う。size limit、chunk budget、early rejectが無いため、peerが巨大な1行または改行しないstreamを送るとmemoryを無界に消費し、OOM/停止でcredentialed orchestration、decision、session controlを失う。

JSON-RPC envelope validationはallocation後なので防御にならない。daemon IPCはlength prefixを先に検査して1MiB超をallocation前拒否しており、stdioだけ境界が欠ける。

### effect-zero 条件

limit超過request/notificationはtool routing、daemon request、store mutationを0件に保ち、buffer/RSSも固定上限内にする。未終端streamで無期限にmemory growthしない。oversize後に同connectionを続けるか閉じるかは明示する。

## 修正方針

- 全行確保前に固定maxを強制するbounded readerへ変える。
- 超過行を有界にdrainして次行へ進むか、stdio connectionをfail-closedで終了するかをprotocol contractとして定める。
- parse/error responseもboundedにする。

## 必要な回帰テスト

`limit-1` / `limit` / `limit+1`、改行なしmulti-chunk、巨大invalid UTF-8、oversize notificationを検証する。各caseでeffect count 0、buffer bound、終了/回復policyを固定する。

## 既存 issue との差分

#482 はJSON-RPC envelope/lifecycle validation、#604 はdaemon pre-handshake frame boundを扱う。MCP stdioのbyte-size admissionはどちらにも含まれない。
