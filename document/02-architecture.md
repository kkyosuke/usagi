# 2. アーキテクチャ

> [ドキュメント目次](README.md) ｜ ← 前へ [1. プロジェクト概要](01-overview.md) ｜ 次へ → [6. 開発規約](06-conventions.md)

v2 の実装は **Cargo workspace 上の 3 クレート＋合成ルート（ルート bin パッケージ）** で構成する。
実行面（TUI / daemon）の境界をクレート境界に一致させ、依存方向を rustc で強制する。
本書がディレクトリ構成・クレート責務・依存ルールの正本である。

## 目次

- [なぜ 3 クレートか](#なぜ-3-クレートか)
- [ディレクトリ構成](#ディレクトリ構成)
- [各クレートの責務](#各クレートの責務)
- [依存ルール](#依存ルール)
- [クリーンアーキテクチャとの対応](#クリーンアーキテクチャとの対応)
- [単一バイナリと合成ルート](#単一バイナリと合成ルート)
- [CI・リリースとの整合](#ciリリースとの整合)
- [実装の置き場所ガイド](#実装の置き場所ガイド)
- [検討した代替案](#検討した代替案)

## なぜ 3 クレートか

v2 は「PTY 所有を daemon に移し、TUI は attach クライアントになる」設計
（[v1/document/proposals/02-daemon.md](../v1/document/proposals/02-daemon.md)）を前提にする。
この設計ではコードが自然に次の 3 つに分かれる。

- **daemon 面**: agent / シェルの PTY 所有・セッション監視・委譲 queue の消化（常駐サーバ側）。
- **TUI 面**: 画面描画・キー入力・attach プロトコルのクライアント側。
- **共通（common）**: 両面が共有する domain エンティティ・usecase・IPC プロトコル型・永続化。

v1 は単一クレート内のモジュール分割だったため、層・面の依存方向はレビューでしか守れなかった。
v2 ではこの 3 分割をクレートとして表現し、「TUI が daemon の内部実装へうっかり依存する」類の
逆流をコンパイルエラーにする。

## ディレクトリ構成

```text
.
├── Cargo.toml            # workspace ルート ＋ 配布バイナリ usagi（bin）のパッケージ
├── src/
│   └── main.rs           # 合成ルート（実 IO の注入と実行面の dispatch のみ。COVERAGE_IGNORE 対象）
├── crates/
│   ├── core/             # usagi-core: 共通（common）
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── domain/          # 純粋エンティティ（他クレート・外部クレート非依存）
│   │       ├── usecase/         # 両面が共有するビジネスロジック
│   │       └── infrastructure/  # 両面が共有する外部接続（IPC プロトコル型・永続化・git）
│   ├── daemon/           # usagi-daemon: daemon 面
│   │   └── src/
│   │       └── lib.rs
│   └── tui/              # usagi-tui: TUI 面
│       └── src/
│           └── lib.rs
└── v1/                   # 退避された旧実装（独立 Cargo プロジェクト。workspace exclude）
```

ディレクトリ名は `crates/<短い名前>`、パッケージ名は衝突回避のため `usagi-<名前>` とする
（`core` は Rust の組み込みクレート名と衝突するため、そのままパッケージ名にしない）。

## 各クレートの責務

| クレート | ディレクトリ | 責務 |
|---|---|---|
| `usagi-core` | `crates/core` | 両面が共有する domain / usecase / infrastructure（IPC プロトコル型・永続化・git） |
| `usagi-daemon` | `crates/daemon` | 常駐プロセス（`usagi daemon`）のサーバ側。PTY 所有・セッション監視・委譲 queue の消化を実装していく |
| `usagi-tui` | `crates/tui` | TUI クライアント側。画面描画・キー入力・attach プロトコルのクライアントを実装していく |
| `usagi`（bin） | ルート | 合成ルート。実 IO（標準入出力・引数・端末）を束ね、実行面へ dispatch する |

## 依存ルール

```text
        usagi（bin, 合成ルート）
          │             │
          ▼             ▼
     usagi-tui     usagi-daemon
          │             │
          └──► usagi-core ◄──┘
```

- `usagi-tui` と `usagi-daemon` は互いに依存**しない**。両面の連携は実行時の IPC だけで行い、
  そのプロトコル型は `usagi-core` が持つ。
- `usagi-core` は他の usagi クレートに依存しない。
- 外部クレートの version はルート `Cargo.toml` の `[workspace.dependencies]` で一元管理し、
  必要になった時点で追加する（v1 の依存を先回りで持ち込まない）。
- lint 設定は `[workspace.lints]` に置き、各クレートは `[lints] workspace = true` で継承する。

## クリーンアーキテクチャとの対応

4 層（`presentation → usecase → domain ← infrastructure`）はクレート分割後も維持する。
層とクレートの対応は次のとおり。

| 層 | 置き場所 |
|---|---|
| domain | `usagi-core` の `domain/` |
| usecase | 両面共有は `usagi-core` の `usecase/`。片面専用のロジックは各面クレート内 |
| infrastructure | 両面共有（IPC プロトコル型・永続化・git）は `usagi-core` の `infrastructure/`。片面専用（PTY は daemon、端末描画は tui）は各面クレート内 |
| presentation | 各面クレート（TUI の画面 / daemon のサーバ端点）と、ルート `main.rs` の dispatch |

依存方向は「クレート間」（tui / daemon → core）と「core 内モジュール」（usecase → domain ← infrastructure）
の両方のレベルで守る。実 IO は合成ルートで注入し、各クレートは依存注入によりユニットテスト可能に保つ。

## 単一バイナリと合成ルート

配布物は従来どおり**単一バイナリ `usagi`** のまま。第 1 引数で実行面を選ぶ
（`usagi daemon` は daemon 面、それ以外は TUI 面）。ルートを bin パッケージとして維持する理由:

- auto-release がルート `Cargo.toml` の `version = "..."` 行を監視しているため、
  version をルートにリテラルで置き続ければリリース機構が無変更で動く。
- release-build-check / release.yml の `cargo build --release` がそのまま root bin をビルドする。
- インストール・利用手順（単一バイナリ配布）が変わらない。

内部クレート（`crates/*`）は `publish = false` とし、`version` を持たない
（配布 version はルートパッケージだけが持つ。version の二重管理によるドリフトを避ける）。

## CI・リリースとの整合

| 対象 | workspace 化との整合 |
|---|---|
| coverage | `cargo llvm-cov --workspace` で crates/ 配下も計測される。`COVERAGE_IGNORE` は合成ルート `src/main.rs` のみ |
| test / clippy | ルートで実行するとルートパッケージしか対象にならないため、`--workspace` を付ける（test.yml / lefthook / recommend-tests の fail-safe も同様） |
| auto-release | ルート `Cargo.toml` にリテラル `version` を維持するため変更不要 |
| release-build-check / release.yml | root bin をビルドするため変更不要 |
| `v1/` | `[workspace] exclude` で計測・ビルド対象外 |

## 実装の置き場所ガイド

v1 から機能を再実装するときの置き場所の指針。

| 実装 | 置き場所 |
|---|---|
| `Workspace` / `Settings` / `Issue` などのエンティティ | `crates/core/src/domain/` |
| `state.json` などの store・IPC プロトコル型・git 操作 | `crates/core/src/infrastructure/` |
| セッション作成・設定解決など両面が使うロジック | `crates/core/src/usecase/` |
| PTY 所有・セッション監視・委譲 queue consumer | `crates/daemon/` |
| 画面・キー操作・attach クライアント | `crates/tui/` |
| CLI 引数解析と実行面の dispatch | ルート `src/`（実 IO の注入のみ。テスト可能なロジックは crates へ） |

## 検討した代替案

構成を決めたときの設計判断の記録。

| 代替案 | 不採用の理由 |
|---|---|
| 単一クレート内のモジュール分割（v1 方式） | 面・層の依存方向をコンパイラで強制できない。ビルド・テストのクレート単位並列性も得られない |
| 層ごとのクレート分割（domain / usecase / infrastructure / presentation を各クレート化） | 実行面（TUI / daemon）の境界を表現できず、daemon 専用と TUI 専用の infrastructure が同じクレートに同居する |
| TUI / daemon を別バイナリとして配布 | リリース CI（4 プラットフォーム）と配布手順の変更が大きい。単一バイナリ＋サブコマンドなら現行リリース機構が無変更で使える |
