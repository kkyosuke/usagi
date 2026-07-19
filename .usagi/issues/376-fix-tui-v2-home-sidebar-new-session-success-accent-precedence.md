---
number: 376
title: fix(tui): v2 Home sidebar の選択中「+ new session」が Success ではなく Accent(青) になる precedence を修正
status: done
priority: medium
labels: [tui, bug, sidebar, switch, closeup, parity]
dependson: []
related: [362, 302, 287]
created_at: 2026-07-19T22:35:54.132573+00:00
updated_at: 2026-07-19T22:53:52.013677+00:00
---

## 背景 / 目的

TUI sidebar の `+ new session` アフォーダンスの色規約は [#302](302-fix-tui-switch-row-v1-dim.md) / [#362](362-fix-tui-sidebar-new-session-success-role.md) で **Success（緑）** に固定されている。規約は次のとおり（`document/03-tui.md` §Home）:

- `+ new session` は**どの mode でも Success（緑）**を保つ。
- **Switch で cursor が乗った選択時だけ太字**にする（色は Success のまま、accent へは落ちない）。
- Switch の cursor でない（非選択）行は他 target と同じ **dim** で描く。
- Closeup では Success（非太字）。

しかし #362 の調査・修正は **v1 の shipped sidebar helper**（`v1/src/.../sidebar.rs` の `create_row` / `rail_create_row` / `panes::overview_preview` / `dim_row`）に閉じていた。#362 は「現行ツリーは全状態で緑を描いており青に落ちる経路は存在しない」と結論したが、これは **v1 tree** の話である。

実行中の **v2 Home view**（`crates/tui/src/presentation/views/workspace.rs::home_row_lines_at`）は別経路で、ここに規約違反がある。

## 現状調査（style precedence の欠陥）

`home_row_lines_at` の label style precedence は次の順で評価される:

```
1. selected                       -> Role::Accent.style().bold()   // ← ここに NewSession が吸われる
2. home.mode == Switch (非選択)    -> dim
3. NewSession                      -> Role::Success.style().bold()  // ← 選択時は到達しない
4. current                         -> Role::Accent.style().bold()
5. それ以外                        -> Role::Accent.style()
```

`selected = home.mode == Switch && home.selected == row`。したがって **Switch で `+ new session` を選択すると分岐 1 に吸われ、Accent（cyan/青）太字で描かれる**。規約が要求する「Success 緑の太字」に到達しない。これが本 issue の主症状。

加えて Closeup（`selected=false` かつ Switch でない）では分岐 3 に落ち `Success **太字**` になるが、規約は「太字は Switch 選択時だけ」＝ Closeup は **Success 非太字**。ここも副次的に規約とずれている。

## 受け入れ条件

- `home_row_lines_at` で `Selection::NewSession` を独立した precedence 分岐として先頭で処理し、次を満たす:
  - Switch + 選択（cursor 上）: **Success（緑）+ 太字**（accent の SGR を持たない）。
  - Switch + 非選択: 既存どおり **dim**（非選択 dim ルールを回帰させない）。
  - Closeup: **Success（緑）非太字**。
- 他 row（session / root）の Accent / dim precedence、cursor（`>` = danger）、active bar（`▎` = success）、inline create input（`+ new: <name>`）、pending skeleton、CJK / tiny geometry を変えない。
- runtime が実際に描く（`render_home` 経由の）選択中 `+ new session` 行が **Success の SGR（`1;32`）を持ち accent の SGR（`1;36`）を持たない**ことを固定する表示回帰テストを追加する。Closeup 非太字（`32` かつ `1;` を持たない）も固定する。既存の Switch 非選択 dim テスト（`\u{1b}[2m+ new session\u{1b}[0m`）を維持する。
- `document/03-tui.md` の記述と実装を整合させる（規約はすでに正しいので、必要なら Switch 非選択が dim になる precedence の明文化にとどめる）。

## 非対象

- v1 shipped sidebar（#362 で対応済み）の再変更。
- 他 row の色・dim 規約の変更、新規モード / レイアウト変更。
