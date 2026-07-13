---
number: 262
title: feat(tui): Home sidebar に animated usagi を表示する
status: done
priority: high
labels: [tui, ui, animation]
dependson: []
related: [225, 259]
created_at: 2026-07-13T00:10:51.914813+00:00
updated_at: 2026-07-13T00:14:33.475818+00:00
---

## 目的

v1 の左ペイン下部にある animated usagi を、v2 Home/sidebar renderer に純粋 render として移植する。

## スコープ

- v1 の mascot 実装・テストを表示内容、frame 遷移、配置の正本として確認する。
- v2 Home/sidebar projection に mascot frame を加え、terminal 非依存の pure render を保つ。
- 既存 Tick / frame 更新経路でのみ animation frame を進め、入力や backend event では不必要に進めない。
- 狭い高さ・幅では sidebar / frame の clipping を守り、Overview 等の modal overlay 中にも背景として残す。

## 受け入れ条件

- Home 左ペイン下部に v1 と整合する usagi が表示され、Tick で決められた順に変化する。
- pure renderer、controller Tick、frame diff / overlay、tiny terminal の regression test を持つ。
- modal の前景・入力優先度を変えず、背景としてのみ見える。
- 実装済み仕様 document を更新する。

## 対象外

実端末への直接書き込み、sleep/thread、daemon state、modal 自体の animation は追加しない。
