---
number: 507
title: fix(daemon): planned replacement を単一 operation に集約し live runtime の cold 破棄を拒否する
status: in-progress
priority: high
labels: [review, v2, daemon, lifecycle, recovery]
dependson: [508]
related: [209, 221, 275, 350, 492, 515, 528, 550, 559]
parent: 505
created_at: 2026-07-21T21:20:49.574125+00:00
updated_at: 2026-07-26T13:29:28.588422+00:00
---

## 実装状況

本 issue は **shipping lifecycle 側の契約**を閉じた。`usagi daemon restart` と `usagi daemon replace` は
`usagi-daemon` の `usecase::replacement` という 1 本の durable operation へ集約され、live runtime を
巻き添えにする cold transition を既定で拒否する。`stop` も同じ census を通る。

**seamless rollover（old process を draining として残す handoff）自体は production からまだ起動できない**。
必要な pure authority（#516 / #518 / #508）は実装済みだが、それを駆動する production 配線
（standby serve と `daemon.lock` の再設計、durable state の owner shard 移行、client の `OwnerRouter` 配線）が
残っている。この残件は
[#559](./559-feat-daemon-standby-serve-owner-shard-seamless-rollover.md) が引き継ぐ。#507 の
`seamless_refusal` は durable registry を実際に読み、欠けている前提を typed に名指すため、
#559 の配線が進むにつれて refusal の理由が変わり、最後に `SeamlessRollover` が選べるようになる。

## 問題・影響

shipping `usagi daemon restart` は [restart usecase](../../crates/daemon/src/usecase/restart.rs) から stop → fresh start を行っていた。旧 daemon process を終了するため、旧 process が所有する PTY master と live Agent / generic Terminal を draining generation として維持できない。fresh daemon の起動時 reconcile は unfinished runtime を `identity_unknown` に落とし、旧 `TerminalRef` を live として復元しない。

さらに悪いことに、この cold restart は **live PTY を黙って破棄していた**。planned restart の state machine と owner fence は存在するが、2 process を同時に安全運用する production authority が無い以上、shipping restart を rollover へ切り替えると二重 active、late spawn、snapshot lost update、誤った owner/capacity release を起こし得る。

## shipping 実装の実証

本 issue 着手時点の production path は次のとおりだった。

- `serve` は process lifetime の `daemon.lock` を保持するため、旧 owner が生存中に standby daemon を同じ data directory で起動できない。
- `restart::restart` は `stop::stop` 完了後に `start::launch_and_confirm` を呼ぶ cold replacement である。
- `GenerationCoordinator::rollover` / `authority::rollover` は shipping lifecycle から呼ばれない。
- `agents.json` / `terminals.json` は process memory から whole snapshot を atomic replace する single-writer store で、cross-process CAS/merge を持たない。
- `usagi daemon replace` は effect-free な trigger を印字するだけで、consumer が存在しなかった。
- client は active locator だけを中心に接続し、draining owner endpoint へ `TerminalRef.daemon_generation` で route しない。

このうち **`replace` の consumer 不在**と **cold restart の無警告な PTY 破棄**を本 issue が閉じ、残りを #559 が閉じる。

## 対象責務（本 issue）

1. manual `usagi daemon restart` と build/update replacement（`usagi daemon replace`）を同じ durable operation へ接続する。`stop` → fresh `start` の bypass を通常経路に残さない。
2. live runtime の census を実測する。exact owner が生存している daemon についてのみ `agents.json` / `terminals.json` の `reserved` / `running` を数え、reconcile 待ちは数えない。
3. seamless rollover の可否を durable registry から導き、typed refusal として提示する。定数ではなく観測から導く。
4. live runtime を持つ daemon の planned `stop` / `restart` / `replace` を effect zero で拒否する。明示的な `--force`（cold transition）のときだけ実行する。
5. development の build-mismatch cold restart は、この guard を意図的に override する経路として `--force` を明示する。

## 非対象

seamless handoff の production 配線（standby serve、owner shard 移行、client owner routing、2 process 実 PTY E2E）は [#559](./559-feat-daemon-standby-serve-owner-shard-seamless-rollover.md)。daemon crash / SIGKILL / OS reboot 後の PTY master fd 回収は #221。

## 受入条件

- [x] `usagi daemon restart` と `usagi daemon replace` は同じ `usecase::replacement` を通り、同じ artifact pair / channel で同じ durable operation ID に attribute される。通常経路に stop → fresh start の bypass が無い。
- [x] live Agent / generic terminal を持つ daemon の planned `stop` / `restart` / `replace` は typed refusal になり、signal も launch も行わず、`daemon.json` / `current.json` / PTY を変更しない。
- [x] refusal は守った内容（Agent runtime 数と generic terminal 数）と、seamless に保てなかった理由を示す。`stop` は successor が存在しないため seamless 理由を報告しない。
- [x] 明示的な `--force` は cold transition を実行し、`stop` は endpoint retire と exact record 消去の完了を確認してから成功を返す。
- [x] daemon が稼働していない、または owner identity を証明できない場合は census を取らず、planned `stop` が「停止するものが無い」状態で拒否されない。
- [x] seamless refusal は durable registry の実観測（不在 / 未対応 schema / 読めない / verified standby 無し / standby はいるが admit する lifecycle が無い）から導く。
- [x] shipping binary・実 daemon process・実 PTY child を使う product E2E が、planned `stop` / `restart` の refusal と `--force` の成功を検証する。
- [x] development の build-mismatch cold restart は `--force` を明示し、guard に阻まれない。
- [ ] → [#559](./559-feat-daemon-standby-serve-owner-shard-seamless-rollover.md): new active の readiness 後の authority handoff、draining owner の維持、owner-generation routing、2 process 実 PTY E2E。

## docs

[daemon](../../document/05-daemon.md) の [planned replacement](../../document/05-daemon.md#planned-replacement) が本契約の正本である。[01-overview](../../document/01-overview.md) の command 表、[04-ipc](../../document/04-ipc.md) の trigger 契約、[03-tui](../../document/03-tui.md) の planned restart 記述を実装済みの現在形に合わせた。
