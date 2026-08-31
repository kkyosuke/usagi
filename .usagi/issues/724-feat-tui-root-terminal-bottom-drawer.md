---
number: 724
title: feat(tui): root terminalを下端drawerに表示する
status: done
priority: medium
labels: [v2, tui, terminal]
dependson: []
related: []
created_at: 2026-08-29T00:00:00+09:00
updated_at: 2026-08-29T00:00:00+09:00
---

## 目的

workspace root の generic Terminal を managed session の Closeup や Agent-only の Director に混ぜず、
Home 下端から表示する専用 drawer として復元する。

## 受入条件

- [x] Home header と `Ctrl-O t` から root shell drawer を開閉できる。
- [x] drawer は画面下端から全幅で表示し、短い端末では header 直下まで広がる。
- [x] Director drawer と排他的に開き、managed pane と各 root surface の選択を保持する。
- [x] daemon inventory の既存 live root terminal を復元・再利用し、drawer は自動で開かない。
- [x] keyboard、pointer、scroll、copy と resize が drawer 専用 viewport へ routing される。
- [x] controller、runtime、rendering、入力 classifier の回帰テストと仕様文書を更新する。
