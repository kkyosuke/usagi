---
number: 492
title: refactor(daemon): GenerationCoordinator を production ownership fencing に統合する
status: done
priority: medium
labels: [review, v2, daemon, generation]
dependson: [458]
related: [209, 365]
parent: 453
created_at: 2026-07-20T12:06:51.807676+00:00
updated_at: 2026-07-21T13:09:22.704119+00:00
---

## 問題・影響

root/v2 の `crates/daemon/src/usecase/generation.rs::GenerationCoordinator` は test 済みだが production constructor 参照がなく、Agent/terminal generation ownership と rollover が複数 runtime の ad hoc map/binding に分散する。restart hydrate #458 後も authority が二重なら stale generation が制御や outcome を誤帰属させる。

## 成立条件 / 再現フロー

production で Agent generation replacement/restart、old terminal command、late outcome を発生させる。`GenerationCoordinator` の fencing test を通らない別 path が状態を決めるため、tested invariant が出荷経路を保証しない。

## 対象責務と非対象

#458 の hydrate 済み Agent state を production generation coordinator へ統合し、ownership/replacement/late event の SSoT を 1 つにする。generic terminal snapshot #459、v1 orchestrator generation #466 は非対象。

## 受入条件

- [x] production composition が coordinator を生成し、全 Agent generation admission/control/outcome を通す。
- [x] restart で durable generation/owner を hydrate し、old/stale ref と late event を effect 0 で拒否する。
- [x] duplicate binding/map を削除し、generation transition を atomic に snapshot と同期する。
- [x] identity unknown は ownership を推測せず fail closed にする。

## 必須回帰テスト

production harness で initial launch、replacement、old ref command、late exit/outcome、restart、corrupt binding、concurrent admission を検証し、単一 authority の transition log を固定する。

## docs / 移行影響

`document/04-ipc.md` / `05-daemon.md` の generation/ownership diagram を更新する。legacy state は #458 の conservative migration に従う。
