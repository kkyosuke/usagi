---
number: 289
title: fix(tui): 候補なしの Tab 補完で入力を維持する
status: done
priority: medium
labels: [bug, tui]
dependson: []
related: []
created_at: 2026-07-13T12:09:48.728610+00:00
updated_at: 2026-07-13T12:12:18.485493+00:00
---

## 背景

Closeup から開くコマンドパレットで `session create a` に Tab を押すと、補完候補がないにもかかわらず入力が `session` に戻る。

## 完了条件

- 候補が 0 件の Tab は入力テキスト、カーソル位置、選択 index を変更しない。
- 既存の一意補完・複数候補の選択・候補表示を維持する。
- `session create a` を含む候補なしの回帰テストを追加する。
- v1 の registry 補完（候補なしで入力不変）と整合する最小修正にする。

## 調査メモ

現行 v2 は専用補完が `None` を返した後、先頭 token に基づくトップレベル候補へフォールバックするため、3 token の入力でも `session` を再適用する。v1 の補完 registry は候補 0 件で元の入力を返す。
