---
number: 655
title: fix(tui): production input adapter を統一し chord・paste・PTY key を欠落させない
status: todo
priority: high
labels: [review, stability, tui, input, bug]
dependson: []
related: [224, 229, 612]
parent: 654
created_at: 2026-08-05T01:16:04.701475+00:00
updated_at: 2026-08-05T01:16:04.701475+00:00
---

## 症状

実端末の crossterm event は `LiveInputClassifier` を経て `Key` へ変換されるが、管理画面用の pure classifier と production adapter が別経路になっており、同じ入力を異なる意味へ写している。

確認できる利用者影響は次のとおり。

- Roles editor は `Ctrl-S: validate + save` と表示するが、production では `Ctrl-S` が `Key::Passthrough` になり Home reducer 前で破棄され、保存できない。
- `Event::Paste` は `Key::Paste` までは保持されるが `app_event_from_key` が一律 `None` にするため、Overview / Closeup prompt、inline session create、workspace Environment、Roles editor へ貼り付けられない。
- `PageUp` / `PageDown` / `Insert` / F1–F12 は `KeyCode` と PTY encoder が存在する一方、production の `passthrough_key` が `Key::Other` へ落とすため、focused live terminal に届かない。
- reducer unit test が使う `classify_management_input` は `Ctrl-S` を `SaveRoles` に写すが、shipping frame loop はこの関数を通らない。

## 原因

入力変換の責務が次の複数箇所に分かれている。

- `src/tui_input.rs`: crossterm → `LiveInput`
- `src/runtime/tui.rs`: `LiveInputClassifier` → `Key`
- `crates/tui/src/presentation/mod.rs::app_event_from_key`: `Key` → `AppEvent`
- `crates/tui/src/usecase/application/controller.rs::classify_management_input`: 別の `LiveInput` → `AppKey` 表

後二者の対応表が production で共有されておらず、pure test が shipping adapter の欠落を検出できない。

## 修正方針

- `LiveInput` から「TUI 管理操作」「focused text editor input」「PTY passthrough bytes」への分類を一つの tested policy に集約する。
- overlay / route / live-pane ownership は分類後に一度だけ適用し、modified key や paste を中間語彙で黙って捨てない。
- Roles の `Ctrl-S`、Home overlay の paste、live PTY の PageUp/PageDown/Insert/F key を明示的な acceptance table にする。
- plain `n`、矢印、Ctrl-C 等の既存 live-terminal/prefix契約と、Director picker の排他的入力所有を維持する。

## 受け入れ条件

- Roles editor を実端末 adapter 経由で開き `Ctrl-S` を入力すると、`SaveRoles` effect が exactly once 発生する。
- Overview / Closeup / inline create / Environment / Roles の各入力欄で paste が入力所有者へ一度だけ届く。live PTY が背後にあっても漏れない。
- focused live terminal へ PageUp/PageDown/Insert/F1–F12 の既定 terminal bytes が一度だけ送られる。
- key release は送らず、Press/Repeat の既存契約を維持する。
- unknown modified chord は管理画面で副作用を起こさず、live pane では raw/portable encodingを保持する。
- fake crossterm source → production classifier → workspace runtime/effect または fake terminal portまでを通す integration testを追加する。最終 `AppKey` を直接注入する testだけで完了扱いにしない。

## docs

`document/03-tui.md` の入力所有・paste・Roles editor・live terminal key forwardingを実装と一致させる。
