---
number: 762
title: refactor(test): 巨大 tests.rs の分割と重複 fake の test_support 集約
status: todo
priority: medium
labels: [v2, refactor, test]
dependson: []
related: [760, 761]
created_at: 2026-09-16T00:00:00+00:00
updated_at: 2026-09-16T00:00:00+00:00
---

## 問題

- `crates/tui/src/presentation/tests.rs` は 23,469 行・334 test の単一ファイルで、workspace 最大のソースである。
  `crates/tui/src/usecase/application/controller/tests.rs` も 8,794 行・191 test ある。
- test double は `Fake*` / `Stub*` / `Recording*` 系で 77 種あり、同名の型が複数箇所で別々に定義されている
  （`FakeClock` 4 箇所、`FakeProvisioner` 4 箇所、`FakeGit` / `FakeConnection` / `FakePort` 各 3 箇所）。
- `usagi-core` と `usagi-daemon` には `test_support.rs` があるが、`usagi-tui` には無く、presentation / controller の
  test が個別に fake を組み立てている。
- nursery lint `clippy::redundant_clone` は test コードにも集中している（`presentation/tests.rs` 31 件）。

1 ファイルが数万行だと、部分的な test 追加でも rustc / rust-analyzer の再解析コストが高く、責務ごとの test を探しにくい。

## 方針

- `presentation/tests.rs` を #758 の分割 module に合わせて `presentation/tests/<context>.rs` へ分ける。
- `crates/tui/src/test_support.rs`（`#[cfg(test)]`）を追加し、fake `Terminal` / clock / port を集約する。
  同じ trait に対する `FakeClock` などは 1 定義に寄せ、crate 間で同じ port を fake する場合は core の
  `test_support` に置く。
- 集約後の fake で未実行の method には inline `#[coverage(off)]` を付け、coverage 100% を保つ。

## 完了条件

- 単一の test ファイルで 5,000 行を超えるものが無い。
- 同名 fake 型の重複定義が無い（`grep -rE 'struct Fake[A-Za-z]+'` の名前が一意）。
