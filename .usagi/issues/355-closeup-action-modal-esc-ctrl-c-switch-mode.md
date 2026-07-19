---
number: 355
title: Closeup action modal の Esc / Ctrl+C を Switch mode 復帰に統一する
status: done
priority: medium
labels: [tui]
dependson: []
related: []
created_at: 2026-07-19T11:02:50.896093+00:00
updated_at: 2026-07-19T12:07:38.927800+00:00
---

## 背景 / 問題

Home の Closeup action modal（`Overlay::Closeup` が開き `CloseupModal` が表示されている状態）で `Esc` / `Ctrl+C` を押したときの挙動が、脱出手段として不十分・非対称だった。

- tab の無い（live pane 無し）Closeup では action modal が base surface で、`Esc` を押しても overlay が開いたままの **dead-end** だった（`Switch` へ戻る手段が `Ctrl-O Ctrl-O` しかない）。
- live pane 上に forced 表示された action modal では `Esc` は modal を閉じて live pane に戻るだけで、`Ctrl+C` は overlay で握り潰されて no-op だった。

## 変更方針

Closeup action modal が表示されている間の `Esc` または `Ctrl+C` を、**modal を閉じるだけでなく `Switch` mode へ遷移**する挙動に統一する（`Ctrl-O Ctrl-O` と同じ着地）。live pane の有無に依らず一律に適用する。

- 対象は `Overlay::Closeup` が開いている場合だけ。`overlay = None` / `route = Home(Switch)` / `closeup_action_forced = false` にする。
- `QuitConfirmation` をはじめとする他 overlay の `Ctrl+C` / `Esc` 契約は変更しない。
  - Closeup mode で **overlay を開いていない** live pane 上の `Ctrl+C` が `QuitConfirmation` を開く契約はそのまま。
  - Closeup overlay 上の `Ctrl+Q` は従来どおり握り潰す（`Ctrl+C` / `Esc` だけを新契約に載せる）。

## スコープ

- `usagi-tui` の controller reducer（`update_overlay` の `Overlay::Closeup`）。
- real terminal 入力経路（`WorkspaceRuntime::handle_key` → `handle_closeup_key`）の回帰テスト。
- `document/03-tui.md` の Closeup action modal 契約の記述更新。

## テスト・確認方法

- controller reducer: Closeup overlay 上の `Escape` / `CtrlC` が `Switch` へ抜けること、`CtrlQ` は inert のままであること、live pane 上の overlay 無し `CtrlC` が `QuitConfirmation` を開く契約が保たれること。
- WorkspaceRuntime（real loop の key 変換）: `Key::Escape` / `Key::Quit`(Ctrl+C) で Closeup modal が閉じ `Switch` へ遷移し `closeup_modal` が破棄されること。
- 既存 parity / table-driven テストの Closeup Escape 期待値を新契約へ更新。
