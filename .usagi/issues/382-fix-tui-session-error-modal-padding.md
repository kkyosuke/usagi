---
number: 382
title: fix(tui): session 作成失敗 error modal の左右内側 padding を対称にする
status: done
priority: medium
labels: [tui, bug, ui, modal, render]
dependson: []
related: [369, 372]
created_at: 2026-07-20T01:05:28.454644+00:00
updated_at: 2026-07-20T01:10:12.771976+00:00
---

## 背景 / 問題

ユーザー報告: session 作成失敗 dialog（`Overlay::CreateSessionError` / `presentation/views/create_session_error_modal.rs`、#369 で追加）の左右の内側余白が非対称に見える。

原因は共通 modal renderer と error modal の折り返し幅の噛み合わせにある。

- 共通の `modal::boxed`（`widgets/modal.rs`）は各本文行を `│ {line}{pad} │` として描く。box そのものは左右**各 1 桁**の対称な余白を与える。
- しかし `create_session_error_modal` は safe message を painting 前に `  {segment}`（先頭 2 桁 indent、共通 `BODY_INDENT` 相当）で組み、折り返し幅を `inner_width - 2` にしている。
- このため box を**埋める幅**まで折り返された行は表示幅が `inner_width` ちょうどに達し、`boxed` の `pad` が 0 になる。結果として、枠いっぱいに折り返された行の**左内側余白 = box(1) + indent(2) = 3 桁**、**右内側余白 = box(1) = 1 桁**となり 3:1 の非対称が生じる（短い行では右に余りがあるため目立たないが、長い safe message が折り返して枠幅に達すると露見する）。

外枠の中央配置（`render_over` の `centered_padding` と垂直中央寄せ）は正しく、問題は内側の左右 padding のみ。

## ゴール

error modal の内側左右 padding を対称（左右とも box(1) + indent(2) = 3 桁）にする。CJK・ANSI・長文エラー・狭い端末でも表示幅と clipping を保ち、confirmation / editor / list など共通 `boxed` を使う他 modal の既存レイアウトは変えない。

## 変更内容

### `presentation/widgets/modal.rs`
- 共通 `BODY_INDENT`（`"  "`）の桁数を表す `pub const BODY_INDENT_WIDTH: usize = 2;` を追加し、`BODY_INDENT` の定義と整合させる（マジックナンバー `2` の意味を 1 箇所に固定）。box の描画（`boxed`）自体は変更しない＝他 modal のレイアウトを壊さない。

### `presentation/views/create_session_error_modal.rs`
- 折り返し幅を `inner_width - 2` から `inner_width - 2 * modal::BODY_INDENT_WIDTH`（= 左 indent 2 桁 + 右に同じ 2 桁を確保）へ変更する。これで枠いっぱいに折り返された行でも右側に 2 桁の空きが残り、`boxed` の box 1 桁と合わせて右内側余白が左と同じ 3 桁になる。
- 折り返し幅の意図（左右対称の内側 padding を確保するため左 indent と同じ幅を右に予約する）をコメントで明記する。

## テスト（pure render regression）
- 長い safe message を枠幅まで折り返させ、最も幅の広い本文行で「左枠から本文までの桁数」と「本文から右枠までの桁数」が一致する（ともに 3 桁）ことを assert する pure render 回帰テストを追加する。
- 既存テスト（`wraps_a_long_message_across_rows_and_shows_all_of_it` / `fits_a_narrow_terminal_without_overflow` / 空 message）が引き続き通り、全文表示・幅超過なし・狭い端末での非破綻を維持する。
- `BODY_INDENT_WIDTH` と `BODY_INDENT` の整合を pin する modal.rs の unit test を追加する。

## ドキュメント
- `document/03-tui.md` の作成失敗 dialog の記述に、safe message を dialog 幅へ折り返す際に左右対称の内側 padding を保つ点を 1 文追記する（表示＝実装の整合）。

## 非スコープ
- 共通 `boxed` / `render_over` の box 幾何そのものの変更（他 modal へ波及するため）。
- inline validation（sidebar の `+ new session` 行下 error）の表示（別 surface。#1097 で対応済み）。
- error modal の発火条件・dismiss・safe 化ロジック（#369 の挙動を維持）。
