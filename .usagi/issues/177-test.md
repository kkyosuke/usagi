---
number: 177
title: test: リスク比例の段階的テスト戦略を導入する（親）
status: done
priority: high
labels: [test, ci, developer-experience]
dependson: []
related: []
created_at: 2026-07-10T23:34:48.985142+00:00
updated_at: 2026-07-11T03:12:12.778377+00:00
---

## 背景

2026-07-11 の実測では、単一 package に 3,363 tests があり、warm `cargo test --quiet` は 104.97 秒。lib unit 3,343 件だけで 117.57 秒（別計測）、`infrastructure::` 51.93 秒、`usecase::` 55.37 秒、`presentation::` 18.71 秒、`domain::` 1.96 秒、`daemon_ipc_test` 11.61〜14.08 秒だった。cold `cargo check --all-targets` は 90.89 秒、warm clippy は 45.04 秒。毎編集で全件を回す運用は feedback を遅らせる一方、PR の全件 gate と coverage 100% は維持する価値がある。

Cargo workspace は実質 `usagi` 1 package のため crate 選択や cargo-hakari は効かない。重い箇所は一時 Git repository/worktree/submodule を作る infrastructure/usecase tests、TUI 描画群、daemon/PTY integration。外部ネットワークを必須とする test、`#[ignore]` された通常 test、明示的 serial-test は確認されなかった（doc test 1 件のみ ignored）。

## 目標

ローカル fast loop を変更リスクに比例させつつ、PR/CI では全件・coverage 100% を required gate として維持する。選択実行は correctness gate の代替にせず、推奨コマンドとして扱う。

## 推奨段階

| 段階 | 実行 | 目安 |
|---|---|---|
| 編集中 | `cargo fmt --all -- --check`; `cargo check --all-targets`; `cargo test --lib <module-path>` または `cargo test --test <target>` | 変更直後。対象 module と直接 consumer を選ぶ |
| commit 前 | fmt + `cargo clippy --all-targets -- -D warnings` + touched module/target tests | 小変更。共通型・trait・永続化形式・feature/target cfg・Cargo files・test infra 変更はここで全 `cargo test` |
| PR/push 前 | `cargo test --quiet`; `. scripts/coverage.sh && coverage_enforce`（coverage が test を兼ねるため重複実行不要） | Rust 差分のある PR は必須 |
| CI | fmt/lint と full test と coverage を独立 check として早期並列実行 | required checks は full test + coverage を維持 |

## 全テスト必須条件

- `Cargo.toml` / `Cargo.lock` / `build.rs` / toolchain、feature、target cfg の変更
- `src/lib.rs` / `src/main.rs` / `src/test_support.rs`、公開 API、共通 trait/type/error、serde/frontmatter/storage format の変更
- `domain → usecase → infrastructure ← presentation` の複数層にまたがる変更
- git/worktree/session/daemon/PTY/thread/process、lock/concurrency、環境変数・global state の変更
- tests/scripts/hooks/CI/coverage 設定の変更、広範 rename/refactor、選択マッピングで未知の path
- flaky/順序依存の疑い、または narrow test が一度でも失敗した変更

## 選択肢比較

| 選択肢 | 速度 | 信頼性 | 導入 | 保守 |
|---|---|---|---|---|
| module filter を手動指定 | 高 | 中（consumer 見逃し） | 低 | 低 |
| touched path→module/target 明示 mapping | 高 | 中〜高（fallback 全件） | 中 | 中 |
| test 全件を毎回 | 低 | 高 | 済 | 低 |
| cargo-nextest | 並列/slow report 改善余地 | 同等（coverage統合要検証） | 中 | 中 |
| cargo-hakari | 効果ほぼ無し（1 package） | 不変 | 中 | 中 |
| cargo llvm-cov | 遅い | coverage gate 高 | 導入済 | 中 |
| path filter で CI full test を skip | 高 | 低〜中 | 中 | 高 |

## ガードレール

- unknown path は全件へ fail-safe fallback。mapping の出力に理由と実行コマンドを表示する。
- PR CI の full test/coverage は path filter で省略しない。docs-only だけ既存の pre-push skip を許す。
- mapping 自体を table-driven test し、module 移動時に更新漏れを検知する。
- draft PR を早期に push し、concurrency cancel-in-progress で古い CI を止める。required checks/branch protection を SSoT とし、merge queue は並行 merge による base drift が実害化してから導入する。
- flaky は retry で隠さず、test name/seed/attempt/time を記録して quarantine issue 化する。

## 非採用（現時点）

- cargo-hakari: workspace 依存 feature の統一が目的で、単一 package の本 repo には過剰。
- cargo-nextest の即時 required 化: まず slow test 計測と CI wall time の比較が必要。daemon/PTY subprocess test の process cleanup と coverage (`llvm-cov --nextest`) を検証してから判断する。
- CI の Rust path filter: 見逃し時に required gate 自体が消えるため採らない。
- merge queue: 現在の規模での base drift/競合 CI の実測がないため先行導入しない。

## 完了条件

子 issue を段階導入し、`.agents/workflow.md` と `document/06-conventions.md` の小変更でも常に全件という規約を、上記の risk-based local verification + full PR gate に更新する。
