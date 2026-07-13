---
number: 290
title: feat(tui): 共通 TextInput を block cursor 表示に統一する
status: done
priority: high
labels: [tui, input, accessibility, parity]
dependson: []
related: [269, 278, 287, 288]
created_at: 2026-07-13T12:09:53.951585+00:00
updated_at: 2026-07-13T12:14:53.459278+00:00
---

## 目的

v2 TUI のフォーカス中で編集可能な 1 行入力を、下線から block cursor へ統一する。対象は Open Filter、New の workspace/session 名入力、Overview palette、Closeup Prompt など、共通 `TextInput` を使う現行の入力欄である。

## 実装

- `presentation::widgets::block_caret` を唯一の描画 API として追加した。入力位置の Unicode scalar を既存の semantic base style と同じ色の reverse-video (SGR 7) で反転し、文字を横へ押し出さない。
- 空文字列と行末には反転空白 1 セルを描く。caret offset は value 範囲と char boundary に正規化するため、全角・Unicode を途中で分割しない。
- `Style::reverse` をテーマへ追加し、New/Open/Overview/Closeup の個別 underline/placeholder 実装を共通 renderer に置換した。非フォーカス値、読み取り専用値、候補/selection の既存 style は変更しない。
- `document/03-tui.md` を、block cursor の意味・empty/end・Unicode・非フォーカス時の規則の正本として更新した。

## v1 との関係

v1 の `block_caret` と同じ reverse-video、末尾空白、Unicode scalar 単位の表示規約を採用した。v2 の実端末 adapter は引き続き hardware cursor を隠しており、IME preedit の cursor parking はこの UI 表示変更の範囲に含めない。

## 完了条件

- 現行の共通 `TextInput` 利用欄は、フォーカス中だけ同一規則の block cursor を表示する。
- 空、先頭・中間・末尾、ASCII、全角 CJK で表示幅と入力文字列を保つ。
- 既存の focus、候補選択、Closeup/Switch prefix、キー入力の reducer 契約を変更しない。
- 実装済み仕様と回帰テストが同じ PR に含まれる。

## テスト

- widget unit: ASCII/CJK、empty/end、reverse SGR、表示幅。
- view regression: New/Open/Overview/Closeup の block cursor。
- `cargo test -p usagi-tui --lib`、workspace check/clippy/coverage、Markdown link check。
