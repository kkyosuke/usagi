---
number: 304
title: feat(tui): 右ペインの tab strip を横スクロール可能にする
status: todo
priority: high
labels: [tui, pane, ux]
dependson: []
related: [256, 279]
created_at: 2026-07-15T00:28:01.531015+00:00
updated_at: 2026-07-15T00:29:54.942921+00:00
---

## 目的

右ペインの tab 数または tab 名が表示幅を超えても、選択中 tab を見失わずに tab strip を横方向へ閲覧・選択できるようにする。

## スコープ

- tab strip に stable tab identity に紐づく viewport / scroll offset を導入する。
- キーボードによる次/前 tab 選択、および利用可能ならホイール等の tab scroll が viewport を更新して選択 tab を可視範囲へ追従させる。
- pending / live / ended、session 切替、tab close、reconnect、terminal resize で offset を正規化する。
- 狭い幅、CJK・wide label、ANSI style、overflow indicator を含む描画を壊さない。

## 受け入れ条件

- overflow する tab strip で前後の tab へ到達でき、選択中 tab は常に見える。
- selection は表示 index ではなく stable identity で保持される。
- tab の追加・削除・切替・resize・session 切替で scroll offset が範囲外にならない。
- reducer / render の table-driven test または golden test で回帰を固定する。

## 関連

#256 / #279 は tab strip と prefix 経由の tab 選択を扱うが、strip の overflow viewport は対象外。
