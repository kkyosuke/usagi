---
number: 69
title: refactor(usecase): session の表示ラベル解決を presentation 層へ寄せる
status: done
priority: low
labels: [refactor, review]
dependson: []
related: [56]
created_at: 2026-06-20T12:04:46.619614+00:00
updated_at: 2026-07-04T00:14:14.820466+00:00
---

## 背景

コードレビューで判明した軽微な層責務の漏れ。`src/usecase/session/mod.rs:165-185` の `set_display_name` が `session.display_label()`（サイドバー表示用ラベル）をドメインに問い合わせ、戻り値として返している。`record`（`mod.rs:138-157`）でも `display_name: None` を初期化する。

`display_name` / `display_label`（override が無ければ name にフォールバック）は本来サイドバー UI（presentation）の関心で、usecase が永続化のついでに「画面に出すラベル」を決定して返すのは層をまたいだ責務の漏れ。`document/06-conventions.md`「層をまたいで書かない」の精神に反する。presentation 側には既に `src/presentation/tui/home/state/list.rs` の `display_label` 経路がある。

## 改善方針

- usecase は `display_name`（生値）の set/clear のみ扱い、表示ラベルを返さない。
- 「display_label = override || name」のフォールバック解決を presentation 側に一本化し、SSoT を崩さない。

## 確認方法

- サイドバーの表示ラベルが従来どおりであること（state / ui テスト維持）。
- `cargo fmt` / `cargo clippy --all-targets -- -D warnings` / `cargo test`（カバレッジ 100% 維持）。

関連: #56（HomeState の表示文字列を ui 層へ退避する流れと整合）。
