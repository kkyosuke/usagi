---
number: 381
title: fix(tui): inline new session 入力中の「+ new」affordance を Success(緑) に固定し色契約をテストで固定する
status: done
priority: medium
labels: [tui, bug, sidebar, parity]
dependson: []
related: [362, 376, 302]
created_at: 2026-07-20T00:56:02.960870+00:00
updated_at: 2026-07-20T01:00:13.379268+00:00
---

## 背景 / 目的

TUI Home sidebar の `+ new session` アフォーダンスの色規約は [#302](302-fix-tui-switch-row-v1-dim.md) / [#362](362-fix-tui-sidebar-new-session-success-role.md) / [#376](376-fix-tui-v2-home-sidebar-new-session-success-accent-precedence.md) で **Success（緑）** に固定されている。しかし #362 / #376 は **静的な `+ new session` ラベル**（`home_row_label` / v1 sidebar helper）に閉じており、両者とも **inline 作成入力（`+ new: <name>`）は明示的に非対象**としていた。

その結果、Switch で `+ new session` を選び Enter/`t` を押して inline 入力欄に展開したとき、行内の `+ new` アフォーダンスだけが **Success 緑にならない**という色の不整合が残っている。静的行は緑なのに、入力に入った瞬間 affordance の緑が失われるため、視覚的な連続性が途切れる。本 issue はこの inline 入力中の `+ new` を Success 緑へ揃え、色契約をテストで固定するのが目的。

## 現状調査（style precedence の欠陥）

inline 入力欄を描くのは `crates/tui/src/presentation/views/workspace.rs::new_session_input_lines`:

```rust
fn new_session_input_lines(width: usize, draft: &CreateDraft) -> Vec<String> {
    let accent = Role::Accent.style().bold();
    let caret = widgets::block_caret(&draft.name, draft.name.chars().count(), &accent);
    let caret_line = format!("{} + new: {caret}", accent.paint(">"));
    ...
```

現在の caret 行の各要素の style:

| 要素 | 現在の style | SGR | 規約上あるべき姿 |
|---|---|---|---|
| cursor marker `>` | Accent bold | `1;36` | 視認性維持（現状維持） |
| `+ new:` affordance | **無指定（端末デフォルト＝無色）** | なし | **Success bold（緑）** |
| name + block caret | Accent bold（`block_caret` の reverse-video 含む） | `1;36` / reverse | 視認性維持（現状維持） |
| validation error（下段） | Danger bold | `1;31` | 現状維持 |

- **`+ new:` は format 文字列内のリテラルで、painted span の間に挟まった無色のプレーンテキスト**である。静的 `+ new session` が Success 緑（`1;32`）なのに、inline に入ると affordance の緑が消える。
- **inline 入力中の affordance 色を固定するテストが存在しない**ため、色を誤って変えても CI が素通りする。

## 受け入れ条件

- `new_session_input_lines` の caret 行で、`+ new` affordance を **`Role::Success.style().bold()`（緑）** で描く。inline 入力は「その行が入力を所有する能動状態」であり、静的 Switch 選択時（Success **太字** `1;32`）と同じ parity にする。
- 次を回帰させない:
  - cursor marker `>`（Accent bold）を維持し、視認性を保つ。
  - name + `block_caret`（Accent bold + reverse-video の block caret）を維持し、CJK・空文字・行末でも caret が 1 セルで描かれる挙動を保つ。
  - validation error は Danger のまま、caret 行の**下へ** sidebar 幅（`unicode-width` 準拠）で折り返す（切り捨てない）挙動を保つ。折り返し行数と `home_row_height_at` の高さ計上の一致を保つ。
  - 各行の `pad_to_width` / `clip_to_width` による幅整合（CJK・styled・narrow-width・ANSI エスケープを 0 桁計上）を保つ。
  - pending skeleton、Closeup / live input ownership、静的 `+ new session`（#362 / #376 で確定済み）の色 precedence を変えない。
- テスト:
  - `new_session_input_lines` 単体で、caret 行の `+ new` affordance が **Success の SGR（`1;32`）を持ち accent（`1;36`）を持たない**ことを固定する。name / block caret（accent）と cursor marker が残ることも確認する。既存の「error 無しは caret 行のみ」「長い error を折り返す」「CJK error 折り返し」テストは維持する。
  - runtime 回帰テスト（`render_home` 経由で inline 作成フォームを開く経路）で、実際に描かれる `+ new:` affordance が **`1;32` を持ち `1;36`（accent）を持たない**ことを固定する。
  - error 表示中でも affordance の緑（`1;32`）と error の Danger（`1;31`）が両立することを確認する。
- ドキュメント（`document/03-tui.md` §Home）に、inline 作成入力の `+ new` affordance も Success（緑）で、静的 `+ new session` と色が連続することを追記し、#302 / #362 / #376 の規約と整合させる。

## 非対象

- 静的 `+ new session`（#362 / #376 で対応済み）の色・dim precedence の再変更。
- cursor marker `>` を danger（静的行の marker 色）へ変える等、affordance 以外の色変更。
- 新規モード・レイアウト変更、profile / model 入力の追加（inline は name-only を維持）。
