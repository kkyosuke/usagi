---
number: 658
title: fix(daemon): terminal observer / PR projection worker の停止を health と shutdown に接続する
status: done
priority: high
labels: [review, v2, daemon, terminal, worker, reliability, shutdown]
dependson: []
related: [555, 649]
parent: 654
created_at: 2026-08-05T13:40:18.929554+00:00
updated_at: 2026-08-05T23:19:39.544632+00:00
---

## Finding（P1 reliability / hidden stop）

最新の worker health は PR refresh / session teardown / custody / retention GC / draining collection / decision maintenance の 6 種だけを列挙する。一方、端末機能の critical pipeline である `usagi-agent-observer`、`usagi-terminal-observer`、`usagi-pr-projection` は `JoinHandle` を即座に捨て、panic/poison/channel-loss で loop が終了しても health bit も daemon shutdown も起こさない。

- Agent observer 停止: PTY output/exit が durable Agent ownerへ届かず、phase/slot/child proof が stale になる。
- terminal observer 停止: generic terminal output/exit が反映されない。
- PR projection 停止: bounded queue の producer は残るが inventory が収束しない。

#649 が直した「daemon は生きているが機能だけ無通知停止」が、より hot な pipeline に残っている。

## 修正方針

- worker vocabulary/registry を maintenance 限定 enum から daemon critical worker の一元 authority へ拡張する。
- panic だけでなく、shutdown/queue close ではない unexpected normal exit（owner mutex poison等）も failure として記録する。
- owner-output observer の停止は継続 serving が安全でないため shared shutdown を要求するか、明示 restart policy を採る。PR projection は少なくとも health danger と bounded producer closureへ収束させる。
- worker handles を lifecycle owner が保持し、shutdown 時に source channel/projection を閉じた後 join する。detached handle を残さない。

## 受入条件

- 3 worker それぞれへ panic / unexpected normal exit を注入し、health または graceful shutdown が決定的に観測できる。
- planned shutdown は failure と誤計上せず、worker が全て join される。
- observer 停止後に PTY reader が永久 block / unbounded queue / retained child proof を残さない。
- metrics schema / health reason は worker count または closed vocabulary を一箇所から投影する。

## 根拠箇所

- `src/runtime/daemon.rs`: `start_agent_observer`, `start_terminal_observer`, `start_pr_projection_worker`
- `crates/daemon/src/usecase/shutdown.rs`: `BackgroundWorker` の6 variant
- `document/05-daemon.md`: worker health contract
