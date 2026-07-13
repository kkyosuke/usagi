---
number: 298
title: TUI 左ペイン下部に daemon metrics を表示
status: done
priority: high
labels: []
dependson: []
related: [297]
created_at: 2026-07-13T22:38:15.314016+00:00
updated_at: 2026-07-13T22:39:47.357954+00:00
---

## 目的

#297 の daemon metrics を TUI で取得し、v1 と同じく左ペイン下部に表示する。

## 受け入れ条件

- metrics event を TUI projection が decode して保持する。
- 左ペインの mascot / footer 領域の直上に最新 daemon metrics を表示し、狭い terminal でも navigation と footer を優先する。
- 未受信・切断・schema 非対応は安全に表示を degrade する。
- 描画と projection をテストし、v2 documentation を更新する。
