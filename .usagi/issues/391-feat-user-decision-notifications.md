---
number: 391
title: feat(tui): user decision request の desktop / notice center 通知
status: done
priority: high
labels: [tui, daemon, ux, notification]
dependson: [379]
related: [28, 329, 330, 379]
created_at: 2026-07-20T04:15:00+00:00
updated_at: 2026-07-20T05:05:00+00:00
---

## 目的

`user_decision_request` の成功を、人が見落とさない desktop 通知と TUI の notice center で知らせる。

## 方針

- daemon の durable pending snapshot を TUI が定期 resync し、workspace fence 済みの新規 decision ID だけを未読として扱う。reconnect/resync の同一 ID は重複通知しない。
- TUI header のベルと未読数をクリックすると、既存 decision modal を開く。header 下の banner は最新の未読を session と summary で示す。
- OS 通知は `DesktopNotifier` port に分離する。macOS は `osascript`、Linux は `notify-send` を固定引数で安全に spawn し、それ以外・コマンド不在・spawn 失敗は TUI を継続する。
- pending decision modal が最前面の場合は入力所有権を奪わず、click/open も既存 overlay 規約に従う。

## 完了条件

- 新規 pending decision が TUI banner、未読 badge、desktop 通知へ一度だけ現れる。
- modal には session と summary が表示され、既存 resolve / dismiss / workspace fence / reconnect の契約を保つ。
- port と reducer の unit test、および v2 正本ドキュメントを更新する。
