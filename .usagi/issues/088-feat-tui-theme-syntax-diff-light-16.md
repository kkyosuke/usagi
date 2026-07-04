---
number: 88
title: feat(tui): ライト端末対応（Theme 設定の配線・syntax/diff の light パレット・16色フォールバック）
status: todo
priority: high
labels: [tui, review]
dependson: []
related: []
created_at: 2026-07-03T23:20:34.162450+00:00
updated_at: 2026-07-03T23:20:34.162450+00:00
---

UI/UX レビュー（2026-07 branch `usagi/ui`）由来。

## 背景 / 問題
1. **Theme 設定が実質何もしない**: Config で Light/Dark/System を変更・保存でき、`document/05-settings.md` は「TUI 全体の配色」と記載するが、presentation 層のレンダリングは `settings.theme` を一切読まない（消費は Config 画面のラベルと CLI 一覧のみ）。「記載＝実装済み」規約に抵触。
2. **ダーク端末前提のハードコード**: シンタックスハイライトが `base16-ocean.dark` 固定（`markdown/highlight.rs`）、diff 背景色が `DIFF_ADD_BG=22` 等「dark terminal 前提」（`home/ui/panes.rs`）。ライト背景端末ではコード・context 行が白背景に溶けて読めない。
3. **16色端末フォールバックなし**: 256 色前提の SGR（`38;5;…`/`48;5;…`）で、特に diff split レイアウトは背景色が唯一の add/del 表現になり 16色端末で区別が消える（unified は `+/-` で救われる）。

## 対応
- `theme.rs` の `Palette` を `Theme` でスイッチ可能にして配線。あるいは当面は Theme 行を設定・ドキュメントから外して issue でロードマップ管理。
- light 用テーマ（`base16-ocean.light` / `InspiredGitHub`）と light 用 diff 背景を用意。自動判定は OSC 11 / `COLORFGBG` 参照を検討。
- 色深度を見て 16色時は通常色 green/red ＋ split でも `+/-` マーカー表示。

## 受け入れ条件
- Theme 設定が実際に配色へ反映される（または設定/ドキュメントから外れている）。
- ライト端末でコードブロック・diff が読める。カバレッジ 100% 維持。
