---
number: 561
title: refactor(daemon): serve を role-aware にして standby process の起動を可能にする
status: todo
priority: high
labels: [review, v2, daemon, lifecycle, durability]
dependson: []
related: [515, 516, 542, 550, 559]
parent: 559
created_at: 2026-07-26T13:58:19.336697+00:00
updated_at: 2026-07-26T13:58:19.336697+00:00
---

## 問題・根拠（コード調査で確定）

`serve` は process lifetime にわたり `daemon.lock` を保持し、それを「1 process 1 data directory」の権威として使う。
そのため **同じ data directory に 2 個目の daemon を起動できず、standby process が存在し得ない**。

[#516](516-refactor-daemon-cross-process-generation-registry-standby-handoff-authority.md) が registry / standby /
handoff / admission の authority を実装済みで、`SecureUnixListener::bind_private` も standby 用に存在するが、
**起動できないので registry に standby が登録されることが無い**。結果として
`replacement::seamless_refusal` は常に `no generation registry` を返す。

## この issue を分けた理由

本 issue は **単一インスタンス性の権威を移す**変更であり、[#559](559-feat-daemon-standby-serve-owner-shard-seamless-rollover.md)
の 3 配線の中で最も安全性に直結する。取り違えると「同じ data directory に 2 個の active daemon」や
「孤児 PTY」を生む。したがって routing 配線（[#560](560-feat-tui-client-ownerrouter-owner-generation-routing.md)）や
durable state 移行と混ぜず、単独でレビュー・検証できる形にする。

## 既存 issue との境界

- [#516](516-refactor-daemon-cross-process-generation-registry-standby-handoff-authority.md)（done）— registry の
  role / readiness / handoff / admission fence の**判定は正本がそちら**。本 issue は production lifecycle の配線だけを行い、
  判定を再実装しない。
- [#542](542-fix-daemon-fence-workspace-mode-home.md)（done）— workspace fence。単一インスタンス性を registry へ
  移した後も、**fence の契約は変えない**。
- [#515](515-fix-daemon-current-locator-crash-safe-atomic-publish.md)（done）— locator の crash-safe publish。
  standby は locator を publish しない（`bind_private`）ことをここで守る。
- [#550](550-fix-daemon-pid-record-lifecycle-command-stale.md)（done）— PID record の lifecycle。
- **rollover の有効化・old generation の回収・2 process の product E2E は [#559](559-feat-daemon-standby-serve-owner-shard-seamless-rollover.md) が持つ**。
  本 issue は「standby を起動して registry に standby として登録し、readiness を立てられる」ところまでで止める。

## やること

1. `daemon.lock` の役割を「1 process 1 data directory」から **registry role の権威**へ置き換える。
   単一インスタンス性は registry の active role と workspace fence で保つ。
2. standby が `SecureUnixListener::bind_private` で private endpoint を bind し、registry へ standby として
   登録して readiness 後に `verified_build` を立てられるようにする。**standby は locator を publish しない**。
3. standby は hydrate を read-only で行い、worker / tick / spawn を開始しない（#516 の contract）。

## 設計上の判断が必要な点

- **どの時点で「active が既に居る」と判定するか**。lock 保持をやめると、2 プロセスが同時に active を主張する窓が
  生じ得る。registry の CAS だけで足りるのか、短命な lock を残して registry commit だけを直列化するのかを決める。
  **窓が残るなら 2 個目の active を fail closed で拒否できることをテストで示す**こと。
- **crash した active の後始末**。lock を process lifetime で持たなくなると、「lock が空いている＝active が死んだ」
  という現在の判定が使えない。custody（`document/05-daemon.md#custody-喪失による-self-shutdown`）と
  registry の stale entry 回収の役割分担を決める。
- **旧 build との混在**。registry を読まない旧 `serve` が同じ data directory に起動した場合の扱いを決める
  （fail closed が既定）。

## 受入条件

- [ ] 同じ data directory に 2 個目の **active** daemon は起動できない（registry と fence で拒否され、typed error になる）。
- [ ] standby は起動でき、private endpoint を bind して registry へ standby として登録され、readiness 後に
      `verified_build` が立つ。**locator は publish されない**。
- [ ] standby は hydrate を read-only で行い、worker / tick / spawn を開始しない。
- [ ] crash した active の stale registry entry が回収され、その後に新しい active が起動できる。
- [ ] `replacement::seamless_refusal` が `no generation registry` 以外の理由を返せるようになる
      （**rollover の有効化そのものは #559**）。
- [ ] カバレッジ 100% を維持する。[document/05-daemon.md](../../document/05-daemon.md) の
      [単一 daemon の 2 段 fence](../../document/05-daemon.md#単一-daemon-の-2-段-fence) を実装に合わせて更新する。

## 必須回帰テスト・計測

- `cargo test -p usagi-daemon`（`usecase::authority` の registry / standby が退行しないこと）
- `cargo test -p usagi --bin usagi`（`serve` の lifecycle 配線）
- **実プロセスの結合テスト**: 同じ data directory へ active を 2 個起動して 2 個目が拒否されること、standby は
  起動できて locator が変わらないこと。起動は必ず
  [`tests/support/daemon.rs` 経由](../../document/06-conventions.md#結合テストからの-daemon-起動)。
- active を SIGKILL した後に stale entry を回収して再起動できることを、write 境界ごとに確認する。
- Rust 差分を含むため fmt / check / clippy / 推奨 test を通し、full gate は PR CI で確認する。
