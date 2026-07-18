---
number: 317
title: feat(tui): PR modal / preview overlay を controller Overlay へ移設する
status: done
priority: medium
labels: [tui, controller]
dependson: [315]
related: [258, 316]
created_at: 2026-07-17T14:22:46.618899+00:00
updated_at: 2026-07-18T05:36:13.965886+00:00
---

## 目的

#258 の runtime 切替（#315）で暫定的に shell overlay として残した PR modal / preview / error modal を、controller の `Overlay` と `Effect` に移設し、shell 暫定 overlay を撤去する。

設計の正本: `.agents/designs/258-controller-runtime-migration.md` §4.4 / §8-1。

## スコープ

- `Overlay::Prs` / `Overlay::Preview` / error 表示を controller に追加し、開閉・キー操作を `update()` で扱う。
- 表示素材の取得（PR 一覧・preview 行）に対応する `Effect` と `BackendEvent` を追加し、`DaemonBackend`（#314）/ `OverlayDataPort` に接続する。
- `render_home` に overlay 描画を統合し、shell 側の `*_modal::render_over` 暫定接続を削除する。

## 完了条件

- 旧経路由来の modal がすべて controller の Overlay / Effect / BackendEvent で表現される。
- reducer テストと `render_home` テストで開閉・表示・Esc 動作が固定される。coverage 100% を維持する。
