---
number: 362
title: fix(tui): sidebar の「+ new session」を Success role に固定し色回帰を防ぐ
status: done
priority: medium
labels: [tui, bug, sidebar, switch, closeup, parity]
dependson: []
related: [302, 287]
created_at: 2026-07-19T20:57:24.820271+00:00
updated_at: 2026-07-19T21:07:58.767503+00:00
---

## 背景 / 目的

TUI sidebar の `+ new session` アフォーダンスの色は、[#302](302-fix-tui-switch-row-v1-dim.md) の色規約で **Success（緑）** と定められている（Closeup では Success、Switch/選択では選択時 Success 太字・非選択は dim）。この緑が「青く見える」表示回帰への耐性を確保し、Success role の一点管理へ寄せるのが目的。

## 現状調査（style precedence）

`+ new session` を描く経路と現在の style:

| 経路 | 関数 | 状態 | 現在の style |
|---|---|---|---|
| Full sidebar 行 | `sidebar::create_row` | selected+overview / それ以外 | 生 `.green().bold()` / 生 `.green()` |
| Rail 行 | `sidebar::rail_create_row` | 同上 | 生 `.green().bold()` / 生 `.green()` |
| 右ペイン preview | `panes::overview_preview`（create 行選択時） | — | 生 `.green().bold()` |
| インライン入力（full） | `chrome::overview_create_rows` | — | `.success().bold()` |
| インライン入力（rail） | `panes::overview_create_pane` | — | `.success().bold()` |
| 非選択 dim（Switch/選択） | `sidebar::dim_row` | in_overview && !selected | 色を剥がし `.dim()` |

- **調査結論: 現行ツリーは全状態で緑を描いており、青（accent=cyan）に落ちる経路は存在しない。**
- ただし `create_row` / `rail_create_row` / `overview_preview` は `theme.rs` が定める意味的 role（`.success()`）ではなく **console の生 `.green()`** を直接呼んでおり、インライン入力（`.success()`）と不整合。`theme.rs` の規約は「view は role を要求し、色は 1 か所（palette）が決める」であり、生 `.green()` は palette 再調整に追従しない“色回帰の温床”。
- **さらに create 行の色を固定するテストが 1 つも無い**ため、誰かが `.accent()`（cyan/青）へ誤って差し替えても CI が素通りする。

## 受け入れ条件

- `create_row` / `rail_create_row` / `overview_preview` の create 行 label を生 `.green()` から意味的 `.success()` role に置き換える（`theme.rs` の `roles_match_their_ansi_colours` が示すとおり出力は現状とバイト一致＝挙動不変）。
- 非選択 dim（`dim_row` 経由）、cursor（`>` = danger）、active bar（`▎` = success）の既存 precedence は変えない。
- runtime が実際に描く create 行（full / rail / preview）が **Success（緑）の SGR を持ち、accent(cyan) の SGR を持たない**ことを固定する表示回帰テストを追加する。
- TUI 色規約ドキュメント（`document/`）に create 行 = Success role を追記し、#302 の規約と整合させる。

## 非対象

- 他の row（session / root）の色や dim 規約の変更。
- 新規モード・レイアウト変更。
