---
number: 589
title: refactor(tui): load_style の色指定を theme Role 経由に統一する
status: done
priority: low
labels: [tui, ssot, theme]
dependson: []
related: []
created_at: 2026-07-30T10:48:07.972770+00:00
updated_at: 2026-07-30T23:23:40.207231+00:00
---

## 背景

`document/02-architecture.md` は `crates/tui/src/presentation/theme` を「色（意味的な役割→具体色）の単一情報源」と位置づけている。

しかし `crates/tui/src/presentation/views/workspace.rs:1028-1035` の `load_style` は、metrics（CPU/メモリ負荷）の hot/busy 表示に theme の `Role`（例: `Role::Danger` / `Role::Warning`）を経由せず、`Color::Red` / `Color::Yellow` を直接指定している。

```rust
fn load_style(value: u64, busy: u64, hot: u64) -> Style {
    if value >= hot {
        Style::new().fg(Color::Red)
    } else if value >= busy {
        Style::new().fg(Color::Yellow)
    } else {
        ...
```

テーマ変更（例: ダークテーマ対応、色調整）を行う際にこの箇所だけ追従しないリスクがあり、正本である theme module の存在意義（「色を変えるならここだけ触ればよい」）を損なう。

## 対象

- `load_style` の `Color::Red` / `Color::Yellow` を、theme の `Role::Danger` / `Role::Warning`（もしくは同等の意味的役割）経由の色に置き換える。
- 他の view/widget に同様の直接 `Color::*` 指定が残っていないか合わせて確認する（本 issue のスコープ内で見つかった範囲のみ）。

## 受入条件

- [ ] `load_style` が `Color::Red` / `Color::Yellow` を直接指定せず、theme の `Role` 経由で色を解決する。
- [ ] 既存の metrics 表示（hot/busy/calm の見た目）に regression がない。
- [ ] `cargo test -p usagi-tui` の関連 golden/render テストが green。
