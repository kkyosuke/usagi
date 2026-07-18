---
number: 347
title: feat(tui): daemon PR snapshot・通知・安全な browser effect を接続する
status: todo
priority: high
labels: [tui, pr, ux]
dependson: [346]
related: [175, 198]
created_at: 2026-07-17T23:45:09.406330+00:00
updated_at: 2026-07-17T23:45:09.406330+00:00
---

## 目的

daemon の PR snapshot/subscription を TUI の sidebar と `p` PR modal に投影し、PR 変化を toast で通知する。Enter、URL 左クリック、PR 行ダブルクリックは TUI client の composition-root `BrowserOpener` effect で既定ブラウザを開く。daemon から browser を起動しない。

## 範囲

- #346 の snapshot を TUI の PR projection として session identity/revision に接続する。
  - 初期表示と reconnect/resync は snapshot を読む。専用 `pr.updated` event は再取得のヒントだけに使い、event payload を画面の正本にしない。
  - stale/duplicate/reordered revision を無視し、選択中 session が消えた場合は modal/sidebar の安全な fallback に収束する。
  - sidebar の PR badge と `p` modal は同じ daemon projection を読む。legacy workspace state / TUI local scanner を authoritative fallback にしない。
- Open/Closed/Merged/Dismissed と title/refresh 状態を表示する。PR の新規検出・title/state 変化には、重複抑制された安全な toast を出す。
- modal の Enter、URL text の左クリック、PR row の double click を同一 `OpenPrUrl` effect に変換する。単クリックの selection、keyboard navigation、Escape close を維持する。
- `BrowserOpener` port を TUI presentation/usecase に導入し、composition root が OS ごとの argv process を選ぶ。
  - macOS: `open <url>`、Linux: `xdg-open <url>`、対応外/失敗: safe toast。
  - shell/interpolation/URL を含む command string を禁止し、canonical `https` URL のみ port へ渡す。
  - browser 起動は daemon、core domain、IPC server に持ち込まない。
- existing `PrModal` の no-op Enter と `SnapshotOverlayData` 依存を、daemon-backed projection/effect に置換する。

## 依存関係

- #346 に依存する（#345 を経由して durable inventory を得る）。
- #198 / #175 は旧 TUI の既存挙動の回帰確認用 related issue で、実装のデータ正本ではない。

## 受け入れ条件

- sidebar と `p` modal が daemon PR snapshot の revision を反映し、`pr.updated` の event を落としても再取得で正しい一覧へ収束する。
- NEW/title/state change の toast は 1 revision につき重複せず、dismissed PR を勝手に再通知しない。
- modal で Enter、URL 左クリック、PR 行 double click の各操作が選択した canonical HTTPS URL を 1 回だけ BrowserOpener に渡す。
- BrowserOpener は argv API だけを使う。URL に shell metacharacter 相当の文字があっても shell を実行せず、canonical validation を通らない値は起動しない。
- browser 起動失敗・unsupported platform・daemon unavailable は TUI を終了させず、安全な toast/fallback を表示する。
- pin/dismiss UI 操作が既存なら daemon mutation/snapshot に接続し、自動 refresh より user action が優先する。未提供の mutation UI を新たに広げない。
- TUI unit/reducer tests と composition adapter tests を追加し、existing keyboard/mouse behaviour の回帰を防ぐ。

## テスト観点

- fake PR snapshot port: initial load、event hint、event loss/reconnect、revision order、focused-session change。
- rendering: state labels（closed を含む）、badge/modal consistency、toast dedupe。
- input: Enter、left click、double click、selection-only single click、Escape/navigation。
- fake BrowserOpener: exact argv/url、invalid/non-HTTPS rejection、failure toast。OS command selection は composition adapter の小さい integration test で確認する。

## 非目標

- URL extraction、durable inventory、`gh` refresh、IPC protocol の変更は #345 / #346。
- daemon からの browser 起動、shell-based opener、URL preview/download は対象外。
