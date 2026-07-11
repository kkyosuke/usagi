---
number: 204
title: docs: sccache CI no-go 後の正本を整理する
status: done
priority: medium
labels: [docs, ci]
dependson: []
related: [203]
created_at: 2026-07-11T07:23:38.138588+00:00
updated_at: 2026-07-11T07:24:38.660475+00:00
---

## 背景

#203 で required Rust gate から sccache を撤去したが、document/07-test-observability.md の sccache opt-in 節には CI 実験を現在実施している記述が残っている。

## やること

- document/07-test-observability.md を現行 workflow に合わせ、CI では sccache を使わないことを正本として明記する。
- CI 実験の詳細は document/proposals/04-sccache-rust-builds.md へのリンクに寄せ、正本側の重複を削る。
- 必要なら document/README.md の説明を更新する。

## 完了条件

- required Test / Coverage、release build check、release workflow、test-metrics で sccache を使わないことが明確である。
- ローカル opt-in helper と benchmark helper の利用手順が維持されている。
- Markdown link check または可能な範囲の軽い検証と git diff --check が通っている。
