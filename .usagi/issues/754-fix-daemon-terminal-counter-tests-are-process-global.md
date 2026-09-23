---
number: 754
title: fix(daemon): 端末カウンタのテストが並列実行で干渉するのを止める
status: done
priority: high
labels: [v2, daemon, terminal, test, flaky]
dependson: []
related: []
created_at: 2026-09-13T00:00:00+00:00
updated_at: 2026-09-13T02:00:00+00:00
---

## 問題

`usecase::terminal::tests::visible_grids_are_refused_before_allocation_or_pty_resize` が CI で
`left: 1051472 / right: 1049552` として失敗し、再実行で成功した。差は 1920 で、これは 80×24 の画面
1 枚分のセル数である。

原因は `RETAINED_SCREEN_CELLS` などが**プロセス全体の atomic counter** であること。テストは

```rust
let before = output_pipeline_counters().retained_screen_cells;
// ... 拒否されるはずの操作 ...
assert_eq!(output_pipeline_counters().retained_screen_cells, before);
```

という形で「この操作はカウンタを動かさない」を確認するが、libtest は同じプロセス内でテストを並列実行する
ため、**別のテストが端末を登録した瞬間に before と after がずれる**。同じ形の assert は同ファイルに複数ある。

無関係な PR を落とす偽陽性であり、再実行で消えるため原因究明の時間も奪う。

## 方針

- グローバルカウンタを読むテストを 1 本の列に載せるか、registry 単位の観測に置き換える。
  - 列にする場合は、カウンタを読む全テストが同じ lock を先頭で取る（重い E2E の直列化と同じ形）。
  - 置き換える場合は、「この registry が保持しているセル数」を registry から読み、プロセス全体の値に
    依存しない assert にする。
- 固定 sleep や retry で隠さない。

## 受入条件

- [x] グローバルカウンタを読む全テストが、並列実行でも他テストの登録・解放に影響されない。
- [x] 対象テストを繰り返し実行しても安定する。
- [x] 干渉が起きる形の assert が新しく増えないよう、契約をテストかコメントで固定する。
