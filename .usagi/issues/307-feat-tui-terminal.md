---
number: 307
title: feat(tui): 右ペインの terminal 出力を選択してコピーできるようにする
status: todo
priority: high
labels: [tui, terminal, clipboard, ux]
dependson: []
related: [248, 265]
created_at: 2026-07-15T00:28:01.651029+00:00
updated_at: 2026-07-15T00:29:55.065334+00:00
---

## 目的

右ペインの terminal / Agent 出力から範囲選択し、OS clipboard へコピーできるようにする。入力 passthrough と競合せず、選択中の画面内容を正確に扱う。

## スコープ

- mouse drag とキーボード代替を含む selection state（anchor / focus / viewport）を terminal input と分離して保持する。
- ANSI 装飾を除いた表示テキストを、行折返し・scrollback・wide character・UTF-8・空白を壊さずコピーする。
- copy 成功 / 失敗を安全な UI feedback で示し、clipboard backend は OS adapter に閉じ込める。
- URL annotation / tab switching / resize / reconnect / output 更新時の selection の扱いを定義する。

## 受け入れ条件

- 右ペインの複数行・CJK を含む出力を範囲選択してコピーできる。
- 通常のキー入力は選択モード外では PTY に一度だけ転送される。
- 選択中の出力更新・scrollback eviction・resize で panic や別内容のコピーを起こさない。
- pure selection / text extraction tests と OS adapter の境界テストを追加する。
