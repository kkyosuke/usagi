# 設計: TUI modal/overlay の共通コンポーネント整理

**対象 issue**: #372（基盤）/ #373（confirmation 統一）/ #374（形別コンポーネント）
**目的**: Home の overlay 群と関連 modal が個別に手組みしている frame・padding・footer/help・confirmation button・scroll の重複を、`widgets/modal.rs` の共通コンポーネントへ寄せて境界を明確化する。各 modal の固有 state・キー・内容は view/controller に残し、表示と入力 semantics は回帰させない。

関連: #261（modal 固定高さ・完了済み）/ #317（PR modal / preview overlay の controller 移設）/ #244・#243（overlay 実装）。

---

## 1. 現状整理

### 1.1 frame primitive は集約済み

`crates/tui/src/presentation/widgets/modal.rs` が枠・配置の primitive を既に持つ。

| primitive | 役割 |
|---|---|
| `boxed(title, inner_width, lines)` | 角丸枠＋タイトル埋め込み |
| `render_modal(h, w, title, inner, body)` | 空画面中央に枠を置く |
| `render_over(h, w, base, title, inner, body)` | 背景を残して中央に合成する |
| `fixed_body(body, body_height)` | body 行数を予約（#261 由来。高さの揺れ防止） |
| `modal_inner_width(width, desired)` | 端末幅に内側幅を clamp |
| `ConfirmationModal` / `ConfirmationView` / `confirmation_buttons` / `render_confirmation_over` | Yes/No confirmation の state と renderer |

### 1.2 controller Overlay と view の対応

Home の一時表示は `Overlay`（`usecase/application/controller.rs`）の 9 種類。各 view renderer が上記 primitive に framing を委譲する。

| Overlay | view file | INNER / BODY | 使う primitive | 形（shape） |
|---|---|---|---|---|
| Overview | `overview_modal.rs` | 56 / 16 | `render_modal` + `render_over` | palette（`TextInput` + 候補 + help + result） |
| Closeup | `closeup_modal.rs` | 50 / 9 | `render_modal` + `render_over` | palette / action menu |
| QuitConfirmation | `quit_modal.rs` | 40 / 3 | `render_over` | confirmation（手組み） |
| Notes | `scratchpad_modal.rs` | 62 / 16 | `render_over` | editor（draft / section / error） |
| Environment | `scratchpad_modal.rs` | 62 / 14 | `render_over` | editor |
| Decisions | `decision_modal.rs` | 62 / 16 | `render_over` | list（一覧）＋ editor |
| Prs | `pr_modal.rs` | 58 / 14 | `render_modal` + `render_over` | list（scroll + detail） |
| Preview | `text_overlay.rs` | 68 / 14 | `render_modal` + `render_over` | text-viewer（scroll） |
| CreateSession | sidebar inline（`new.rs` は full-screen） | — | — | inline 入力（modal ではない） |

同型で Overlay 外の関連 modal:

- `remove_modal.rs`（52 / 14）— multi-select list。`Overlay` variant を持たない別フロー。
- `open.rs` — full-screen 画面。`ConfirmationModal` state を持つが unregister / cleanup prompt は手組み。共通 `render_confirmation_over` を唯一使うのは open の "Unregister workspace"。

full-screen 画面（`config.rs` / `new.rs` / `open.rs`）は `mascot_screen` 経由で base なしに描画され、overlay ではない。今回の主対象は overlay と同型 modal。

### 1.3 重複の棚卸し（減らす対象）

frame primitive の 1 段上、「body の組み立て約束事」が各 view に散在している。

1. **footer/help 行** — ほぼ全 modal が `Style::new().dim().paint("  …: …   …: …")` を最終行に push し、直前に空行 spacer。leading space の有無・区切りが不揃い。`remove_modal` は help を先頭に置く。
2. **2 桁 body indent** — `clip_to_width(&format!("  {line}"), INNER_WIDTH)` が decision / scratchpad / pr / overview / closeup / remove に散在。
3. **空状態 notice** — `dim().paint("  (none)")` / `(empty)` / `no pull requests` など 6+ か所。
4. **scroll indicator** — `↑ N more` / `↓ N more`（wording 差）が pr_modal と text_overlay に独立実装。
5. **`render` / `render_over` 双子** — `render_modal` か `render_over` かだけ違う boilerplate が pr / overview / closeup / text_overlay に重複。
6. **小端末 reserve** — `fixed_body(body, BODY_HEIGHT.min(height.saturating_sub(4)))`（text_overlay は `-2`）が同一コメントごと copy-paste。
7. **選択マーカー `›`** — `Role::Danger.bold().paint("›") else " "` が pr / overview / closeup / remove / decision と `widgets/select.rs` に重複。
8. **confirmation 未統一** — Quit と open cleanup は手組み。共通 renderer は open unregister のみ。
9. **ANSI strip test helper** — pr / overview / closeup の test module に verbatim コピー。

---

## 2. 目標アーキテクチャ

境界の原則:

- **view/controller に残す**: 各 modal の state、キー解釈、そして「何を表示するか」（どの行・どの text）。
- **共通コンポーネントへ移す**: 枠、padding/indent、help/footer、空状態 notice、scroll indicator、body 予約、小端末 clamp、confirmation button、選択マーカー。

```
                  ┌──────────────────────────────────────────────┐
                  │ widgets/modal.rs（frame primitive・既存）      │
                  │  boxed / render_modal / render_over            │
                  │  fixed_body / modal_inner_width                │
                  └───────────────▲──────────────────────────────┘
                                  │ 載る
          ┌───────────────────────┴───────────────────────┐
          │ body-composition kit（#372・新規）             │
          │  footer / heading / caption / empty_notice     │
          │  content_line / scroll_indicator / marker      │
          │  render_body / render_body_over（双子＋reserve）│
          └───────▲───────────────▲───────────────▲────────┘
                  │               │               │ 載る
        ┌─────────┴───┐  ┌────────┴──────┐  ┌─────┴─────────────┐
        │ confirmation │  │ 形別 component │  │ （既存 view は kit │
        │ 統一（#373） │  │（#374）        │  │  を直接利用）      │
        │ Quit / open  │  │ list/text-     │  └───────────────────┘
        │              │  │ viewer/editor/ │
        │              │  │ palette        │
        └──────────────┘  └────────────────┘
```

### 2.1 形別コンポーネント境界

| shape | 対象 | 共通化する部分 | 固有に残す部分 |
|---|---|---|---|
| **confirmation** | Quit / open unregister・cleanup | Yes/No button・キー hint・role・見出し/本文枠 | 文言・確定効果 |
| **list** | Closeup / Prs / Decisions(一覧) / remove | 選択行・カーソルマーカー・scroll viewport・footer | 行の中身・選択の意味 |
| **text-viewer** | Preview（+ PR error の Unavailable） | 縦 scroll・scroll indicator・footer | 表示 text・dismiss 挙動 |
| **editor** | Notes / Environment / Decisions(editor) | draft 行・section 切替・error 行・footer | field 構成・保存効果 |
| **palette** | Overview / Closeup(prompt) | `TextInput` 入力行・前方一致候補・usage/help・result strip・footer | command registry・実行効果 |

---

## 3. 段階（minimal safe）

いずれの段も「表示は回帰させない」を守り、移行前後で同一 base・同一 state に対する render frame の一致を test で固定する。

### 3.1 #372 — body-composition kit（基盤）

`widgets/modal.rs`（必要なら `widgets/modal/` へ分割）に pure helper を追加し、対象 view を寄せる。想定 API（名称は実装時調整可）:

- `footer(hints)` / `heading(text)` / `caption(text)` / `empty_notice(text)`
- `content_line(text, inner_width)`（2 桁 indent + clip）
- `scroll_indicator(above, below)`（wording 統一）
- `selection_marker(selected)`（`widgets/select.rs` と整合）
- `render_body` / `render_body_over`（双子 + `min(height-4)` reserve を畳む）

**表示は byte 単位で不変**。意図的な統一（footer 文言・マーカー）が必要な箇所のみ本 doc に明記し test を更新。低リスクで #373 / #374 の土台になる。

### 3.2 #373 — confirmation 統一（#372 依存）

`ConfirmationView` を parametrize（button ラベル・footer キー hint・role・見出し/本文、単一キー hint の compact variant）し、Quit を共通 renderer へ移行。Quit の copy（`Detach from this workspace?` / `y: detach` / `n / Esc: stay`・danger 強調）を parametrization で保持し**回帰させない**。open の cleanup prompt も寄せられる分は移行。reducer の Yes/No・Enter/Esc/y/n 挙動は不変。

### 3.3 #374 — 形別コンポーネント（#372 依存）

list / text-viewer / editor / palette の composition helper を定義し、pr_modal・text_overlay の scroll viewport 計算を 1 本化、list 系のマーカー・行 clip・footer を寄せる。各 modal の state・キー・内容は view/controller に残す。diff が大きい場合は shape 単位で PR 分割（各段は #372 に依存、shape 間は独立）。

### 3.4 依存関係

```
#372（基盤）── #373（confirmation）
            └─ #374（形別コンポーネント）
#373 と #374 は互いに独立。
```

---

## 4. 非目標

- modal の幅・配色・入力 semantics・key binding・daemon request の変更。
- `CreateSession` の modal 化（sidebar inline のまま。#361 の決定を維持）。
- full-screen 画面（config / new / open 本体）の描画経路変更。

---

## 5. 検証

- 各段で「移行前後の frame 一致」test（representative state transition ごと）。
- tiny terminal（幅 1〜9・高さ極小）で panic / out-of-bounds / 背景合成範囲逸脱がないこと（既存 tiny-terminal test を維持・拡張）。
- coverage 100%（CI 強制）。pure helper は fake/直接値 test で覆い、`#[coverage(off)]` は実 IO / 単相化重複に限る。
- `document/03-tui.md` の modal 節に共通 body-composition と形別コンポーネント境界を追記。
