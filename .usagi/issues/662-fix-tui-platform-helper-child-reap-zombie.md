---
number: 662
title: fix(tui): platform helper child を非同期に reap して zombie を残さない
status: in-progress
priority: medium
labels: [review, v2, tui, process, resource, reliability]
dependson: []
related: [391]
parent: 654
created_at: 2026-08-05T13:49:12.964021+00:00
updated_at: 2026-08-05T23:05:43.364797+00:00
---

## Finding（P2 process / resource leak）

TUI compositionのdesktop notification、browser open、external terminal openは `Command::spawn()` 後に `Child` handleを即dropする。Rustの `Child` dropはwait/reapを行わないため、Unixではhelperが終了するとTUI process終了までzombieとして残り得る。decision通知は新規decisionごと、browser/linkはclickごとに発生するので、長寿命TUIでprocess table entryを蓄積できる。

## 修正方針

- platform process adapter共通のnon-blocking reaperをcomposition rootに置き、spawn成功した全helper `Child` をmoveして必ずwait/reapする。
- TUI threadはwaitしない。reaperのtracked child数/queueをhard boundし、満杯時は新規helper spawnをsafe failure/no-opとして扱い、handleをdropしてzombie化しない。
- long-lived external-terminal launcherと短命notification/browser helperを区別し、1つのlong-lived childが後続reapを直列に塞がない設計にする（bounded set + `try_wait` 等）。
- shutdown時は新規admissionを止め、終了済みchildをreapする。live external appをkillする責務は持たない。

## 受入条件

- N回の短命notification/browser fixture後に全childがreapされ、tracked setがbaselineへ戻る。
- long-lived helperが1件あっても短命helperのreapが進む。
- queue/cap超過でthread/Child handleを無制限生成せず、TUI input/renderをblockしない。
- fixed argv/no-shellとnotification best-effort、browser safe feedback、external terminal success/error契約を維持する。

## 根拠箇所

- `src/runtime/tui.rs`: `PlatformDesktopNotifier`, `PlatformBrowserOpener`, `PlatformExternalTerminalPort`
