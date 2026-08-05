---
number: 646
title: fix(daemon): unverified record を lock-fenced recovery で回収する
status: done
priority: high
labels: [v2, daemon, lifecycle, recovery, process-safety]
dependson: []
related: [515, 550]
created_at: 2026-08-05T00:00:00+09:00
updated_at: 2026-08-05T00:00:00+09:00
---

## 問題・影響

`DaemonRecord.process_start_identity` を持たない legacy record は、recorded PID の実 owner を証明できないため
`Unverified` となる。現行 lifecycle はこの判定を signal 可否だけでなく stale endpoint / record の回収可否にも使い、
`daemon start` / `stop` / `restart` と ordinary bootstrap を effect zero で拒否する。

daemon がすでに終了して `daemon.lock` を解放していても、PID が別 process に再利用されると legacy record は永久に
`Unverified` のままになる。その結果、到達不能な `current.json` / generation socket / `daemon.json` が残り、TUI は
`daemon unavailable` と `synchronization failed` を繰り返す。利用者が durable session state を避けて手作業で lifecycle
artifact を削除しなければ復旧できない。

## 修正方針

- process identity は **signal authority** として使う。`Exact` の owner だけを signal し、legacy / unknown / mismatchへ
  raw PID signal を送らない。
- stale recovery は **reclaim authority** として別に判定する。ordinary bootstrapではvalidated endpointが到達不能で、
  lifecycle commandでは利用者が明示操作し、かつ`daemon.lock`を取得してlock下でlifecycle record全体が同一であることを
  再確認できた場合だけ、socket-first / locator-lastでendpointをretireし、
  exact recordをCAS clearする。
- lockがbusy、recordが変化、locatorがmalformed / unsafe、cleanupがpartial failureなら全artifactを保持する。
- 旧実装がprocess umaskで残したexact `0644` のowner-owned regular single-link `bootstrap.lock` / `daemon.lock`は、
  descriptor identityを検証して`0600`へ狭める。hardlink、symlink、非regular node、別owner、他のbroad modeは拒否する。
- `start` / `restart` / ordinary bootstrap は同じrecovery primitiveを使う。`stop`はunverified ownerへsignalせず、上記proofが
  成立した場合だけ回収する。
- `status`はprocess ownershipが未検証であることに加え、recovery proofをまだ試していない状態であることを表示する。

## 受入条件

- [x] identity欠落record + 到達不能endpoint + 取得可能な`daemon.lock`は、無signalでendpointとexact recordを回収できる。
- [x] 上記状態から`start` / `restart` / ordinary bootstrapがreplacementを1つだけ起動する。
- [x] lock busy、record差し替え、unsafe locator、cleanup failureではeffect zeroまたはcommit済み段階だけとなり、
      live owner / replacement artifactを破壊しない。
- [x] unverified recordを占有するPIDへsignalを送らない。
- [x] lifecycle commandとordinary bootstrapが同じrecovery verdictを使う。
- [x] production lifecycle testでlegacy recordとPID再利用を作り、`daemon unavailable`から自動復旧する。

## docs / gate

`document/05-daemon.md`のlifecycle判定表をsignal authorityとreclaim authorityに分離する。process / socket / durable
stateへ影響するためfmt、workspace check / clippy、selected testsを実行し、full test / coverage 100%はPR CIで確認する。
