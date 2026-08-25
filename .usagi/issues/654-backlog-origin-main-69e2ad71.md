---
number: 654
title: backlog: origin/main 69e2ad71 最新コードレビュー
status: done
priority: high
labels: [review, v2, backlog, epic]
dependson: []
related: []
created_at: 2026-08-05T13:39:21.130662+00:00
updated_at: 2026-08-07T02:52:01.958771+00:00
---

## レビュー基点

- reviewed commit: `69e2ad71329e2c6eedafab178d32421fa71699b3`
- reviewed at: 2026-08-05
- 観点: 最低限利用可能な操作フロー、フリーズしない UI、メモリ・描画・常駐 worker の無駄、SSoT と責務分離

## Finding 対応表

| priority | issue | invariant |
|---|---:|---|
| high | #655 | live VT parser の CSI / SGR state と自己生成 checkpoint を bounded にする |
| high | #656 | Agent readiness subprocess を owner lock 外で bounded に完了・回収する |
| high | #658 | terminal observer / PR projection worker の停止を無通知にしない |
| medium | #659 | scrollback 上限到達後の oldest eviction を O(1) にする |
| medium | #660 | idle tick の session / git / terminal projection clone・scan を change-driven にする |
| medium | #661 | Agent CLI availability / version probe の process policy を一元化し bounded にする |
| medium | #662 | notification / browser / external-terminal helper child を zombie にしない |
| medium | #663 | frame diff grid の per-cell String allocation を style/run 数へ縮退する |

## 結論

現行 `origin/main` は workspace build / clippy を通る一方、長時間 IO を TUI thread / daemon owner lock 上で実行する経路、live VT parser の未 bounded state、redraw skip より前の全 projection clone、critical worker の未監視が残る。上表の子 issue を優先度順に修正する。

## 完了条件

- すべての子 issue が `done` になり、各 issue の selected test と PR CI full gate が green。
- TUI の長時間操作が入力・描画を block せず、daemon の critical pipeline が無通知停止しない。
- live terminal の memory / scrollback / frame material cost が明示した bound または revision に従う。
- provider 語彙・probe policy・描画 projection の authority が二重定義されない。
