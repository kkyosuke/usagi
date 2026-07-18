# 6. 開発規約

> [ドキュメント目次](README.md) ｜ ← 前へ [2. アーキテクチャ](02-architecture.md) ｜ 次へ → [7. MCP サーバ](07-mcp.md)

v2 の開発で守るべき規約。**開発者・AI エージェントの双方**が従う。
プロジェクト全体像は [1. プロジェクト概要](01-overview.md) を参照。

## 目次

- [アーキテクチャ](#アーキテクチャ)
- [依存クレート](#依存クレート)
- [ブランチ名](#ブランチ名)
- [コミットメッセージ](#コミットメッセージ)
- [プルリクエスト](#プルリクエスト)
- [ドキュメント規約](#ドキュメント規約)
- [品質チェック（リスク比例の gate）](#品質チェックリスク比例の-gate)
- [変更箇所からの推奨テスト](#変更箇所からの推奨テスト)
- [Git Hooks（lefthook）](#git-hookslefthook)
- [CI（GitHub Actions）](#cigithub-actions)
- [リリース](#リリース)

## アーキテクチャ

4 クレート（`usagi-core` / `usagi-daemon` / `usagi-tui` / `usagi-cli`）＋合成ルートの Cargo workspace で、
各クレート内はクリーンアーキテクチャの依存方向（`presentation → usecase → domain ← infrastructure`）を守る。
**構成・責務・依存ルールは [2. アーキテクチャ](02-architecture.md) が正本**。開発時は次の 3 点だけ守ること。

- `usagi-tui` / `usagi-daemon` / `usagi-cli` を相互に依存させない。プロセス内の面選択は
  合成ルートが要求型を変換し、daemon との実行時通信は `usagi-core` の IPC プロトコル型を介する。
- `usagi-core` の `domain/` は他層・他 usagi クレートに依存させない。外部クレートは時刻（`chrono`）と (de)serialize 語彙（`serde`）の基盤語彙に限り、git・PTY・IO 等の重い外部クレートは持ち込まない（正本は [2. アーキテクチャ#依存ルール](02-architecture.md#依存ルール)）。
- 依存方向を逆流させない（例: `usagi-core` から実行面クレートを参照しない）。

## 依存クレート

外部依存は**必要になった時点で追加**する（v1 の依存を先回りで持ち込まない）。version は
ルート `Cargo.toml` の `[workspace.dependencies]` で一元管理し、各クレートは
`<crate>.workspace = true` で参照する。

現在追加済みの外部依存は次のとおり。

| クレート | 使途 | 種別 |
|---|---|---|
| `chrono` | domain エンティティの時刻 | 本依存 |
| `serde` | エンティティ・インデックスの JSON (de)serialize derive | 本依存 |
| `uuid` | v2 resource incarnation の typed ID（UUIDv4）と durable operation ID（UUIDv7） | 本依存 |
| `serde_json` | `index.json` / `workspaces.json` / `daemon.json` の (de)serialize、`usagi-cli` の MCP サーバの stdio JSON-RPC、`usagi-daemon` の IPC メッセージの wire JSON | 本依存 |
| `anyhow` | infrastructure（永続化ストア）のエラー伝播 | 本依存 |
| `fs2` | ストアの cross-process ロック、および合成ルートでの daemon 単一インスタンスロック（`flock` 相当） | 本依存 |
| `dirs` | 既定データディレクトリ（`~/.usagi`）の解決 | 本依存 |
| `rayon` | markdown ファイルの並列スキャン | 本依存 |
| `unicode-width` | 端末描画の表示桁数測定（CJK など全角の 2 桁計上） | 本依存 |
| `clap` | 入口面 CLI の引数解析（コマンドツリー定義） | 本依存 |
| `clap_complete` | `usagi completion <shell>` のシェル補完スクリプト生成 | 本依存 |
| `crossterm` | 対話 TUI の実端末バックエンド（raw mode・代替スクリーン・キー/リサイズイベント） | 本依存 |
| `libc` | 合成ルートでの daemon の pid 生存判定（Unix の signal 0） | 本依存 |
| `tempfile` | ストアのユニットテスト用の一時ディレクトリ | dev |

`usagi-core` の `domain/`（`Workspace` / `Issue` / `Memory` / `DaemonRecord` / `Recent` / typed ID …）は
`chrono` / `serde` / `uuid` だけを使う。`serde_json` / `anyhow` / `fs2` / `dirs` / `rayon` は
`infrastructure/`（永続化）が使い、`serde_json` は加えて `usagi-cli` の MCP サーバ（stdio
JSON-RPC）と `usagi-daemon` の IPC メッセージ (de)serialize でも使う。`unicode-width` は
`usagi-tui`、`clap` / `clap_complete` は `usagi-cli` が使う。
`crossterm`（実端末 IO）・`libc`（daemon の pid 生存判定）・`fs2`（daemon 単一インスタンス
ロック）は合成ルート（`src/main.rs`）も使い、`usagi-tui` は `Terminal` ポートに対して純粋に振る舞う
（[2. アーキテクチャ#依存ルール](02-architecture.md#依存ルール)）。

## ブランチ名

`main` または `<type>/<説明>`。

- type: `feat|fix|docs|refactor|perf|test|build|ci|chore`
- 例: `feat/add-doctor-command`
- pre-commit フックで命名規則がチェックされる。
- **例外**: usagi のセッション worktree（`.usagi/sessions/<name>/`）はブランチ名が `usagi/<name>` になる。`usagi` は許可された type ではないため `<type>/<説明>` を満たせず、pre-commit フックはこの worktree 内のコミットを命名規則チェックの対象外にする（判定は worktree のパスが `.usagi/sessions/` 配下かどうかで行う）。

## コミットメッセージ

[Conventional Commits](https://www.conventionalcommits.org/ja/) 形式。`<type>[(scope)][!]: <説明>`。

- type: `feat|fix|docs|style|refactor|perf|test|build|ci|chore|revert`
- 例: `feat: doctor コマンドを追加` / `fix(cli): 引数解析のエラーを修正`
- commit-msg フックでチェックされる。

## プルリクエスト

- タイトルは Conventional Commits 形式に合わせる。
- 本文には「目的 / 変更内容 / テスト・確認方法」を含める。
- ベースブランチは `main`。[CI](#cigithub-actions) が強制する。

## ドキュメント規約

`document/` 配下・`README.md`・`.agents/` を書くときのルール。**実装を変えたら同じ PR で対応ドキュメントも更新する**
（[ワークフロー](../.agents/workflow.md) ステップ 3）のが大前提で、その上で次を守る。

### 記載＝実装済み

- **現在のビルドで動作する仕様だけを書く**。未実装・予定の機能、「coming soon」「移植予定」、`✅` / `🚧`
  などの実装状況マーカーは置かない（あると「どこまで本当か」を読者が判断できなくなる）。
- 記述は**現在形・断定形**で書く（「実装します」「移植していきます」ではなく「〜する」「〜である」）。
- ロードマップを残したい場合は、本仕様ドキュメントと混ぜず別管理にする（issue ストア `.usagi/issues/`）。

### SSoT（単一情報源）

- **1 つの事実は 1 か所だけに書く**。重複しそうな内容は**正本**を 1 つ決め、他のドキュメントはそこへリンクする。
  正本側には「ここが正本」と明記する。
- 主な正本の所在:

  | 内容 | 正本 |
  |---|---|
  | workspace 構成・クレート責務・依存ルール | [02-architecture.md](02-architecture.md) |
  | 開発規約 | 本書（06-conventions.md） |
  | v1 時点の仕様（コマンド・画面・データ構造・orchestration） | [v1/document/](../v1/document/README.md)（退避版。更新しない） |

- **層をまたいで書かない**。v2 の実装が増えて仕様ドキュメントを追加するときも、1 つの事実の置き場所を
  1 か所に保つ。

### 構造

- **1 ファイル = 1 トピック**。番号付きファイル（`01-` …）で構成し、番号は v1 の `document/` と
  同じ体系を使う（[目次](README.md) 参照）。
- ファイルが長くなりすぎたら分割する（目安: 1 ファイル 300 行を超えたら要検討）。実装の内部詳細（コード構造・
  拡張点）は仕様ドキュメントに書かず、`02-architecture.md` か該当コードへのポインタにとどめる。

### ナビゲーション

- 各ファイルの先頭に `> [目次] ｜ ← 前へ […] ｜ 次へ → […]` のパンくずを置く。
- 章の冒頭に**目次**（`##` 見出しへのアンカーリンク）を置く。

### 可読性

- **列挙・対照は散文でなく表**で、**フロー・階層は ASCII 図**で示す。
- **テーブルのセルに段落を詰め込まない**。詳細な挙動は、表の下に節を設けて書く。
- 型表記は `string?`（Optional）のように統一する。

### リンク

- ディレクトリ内・ディレクトリ間とも**相対リンク**を使う。リンク切れと**見出しアンカー**（`#見出し`）は
  CI（[markdown-link-check](#cigithub-actions)、lychee）で検証されるため、目次・アンカーは見出しと一致させる
  （不一致は CI 失敗）。
- ソースコードは `path:line` で固定参照せず、該当する仕様ドキュメントへリンクする（行番号は陳腐化しやすい）。

## 品質チェック（リスク比例の gate）

検証 gate は「編集中の fast loop」「commit 前」「push / PR 前・CI」に分ける。この節が、開発者・AI
エージェント双方の品質チェックの正本である。workspace 構成のため、test / clippy / check には
**必ず `--workspace` を付ける**（ルートで実行するとルートパッケージしか対象にならない）。

| 段階 | 必須 gate | コマンド |
|---|---|---|
| 編集中 | フォーマット差分の確認 / コンパイル確認 / 変更 crate・module の test | `cargo fmt --all -- --check` / `cargo check --workspace --all-targets` / 変更箇所に対応する `cargo test -p <crate>` |
| commit 前 | Lint / risk-based selected tests | `cargo clippy --workspace --all-targets -- -D warnings` / `scripts/recommend-tests.sh origin/main` が示す test（または同等以上の理由付き selected tests） |
| push / PR 前 | Rust full gate / Markdown link check | Rust 差分あり: `cargo clippy --workspace --all-targets -- -D warnings` と `cargo llvm-cov --workspace --no-clean --fail-under-lines "$COVERAGE_MIN" --fail-under-functions "$COVERAGE_MIN"`。Markdown 差分あり: `lychee --config lychee.toml --no-progress '*.md' 'document/**/*.md' 'v1/README.md' 'v1/document/**/*.md' '.agents/**/*.md' '.github/**/*.md'` |
| CI | PR gate | `.github/workflows/test.yml` が fmt / clippy / `cargo test --workspace --quiet`、`.github/workflows/coverage.yml` が coverage 100%、`.github/workflows/markdown-link-check.yml` が Markdown link check を実行する |

push / PR 前の coverage は次のローカル経路で実行してよい。`cargo llvm-cov` はテスト実行を兼ねるため、この経路では
同じ差分に対して `cargo test --workspace --quiet` を重複実行しなくてよい。

```bash
. ./scripts/coverage.sh
coverage_enforce
```

docs-only（Rust 差分なし）は Rust gate（`cargo check` / `cargo clippy` / `cargo test` / coverage）を省略できる。ただし
Markdown 差分を含むため、Markdown link check は必須である。

full test / coverage gate を必須とする条件は次のとおり。

- push / PR 前または CI で Rust 差分（`*.rs`、`Cargo.toml`、`Cargo.lock`、Rust の build / test / coverage に影響する `scripts/`・`.github/workflows/`・hook）を含む。
- docs-only を除き、`scripts/recommend-tests.sh` が fail-safe として `cargo test --workspace --quiet` を推奨する（未知の path、空 diff、複数クレートにまたがる変更、共有基盤の変更など）。
- 変更がクレート境界・層境界、永続化、process / PTY / terminal IO、設定解決、テスト基盤、coverage 除外、CI / hook の gate に影響する。
- selected tests で対象リスクを説明できない、または直接 consumer を特定できない。

- テストカバレッジ 100% を維持する（CI / lefthook でチェック）。
  - **依存を注入してテスト可能にする**。「テストできないから」とロジックを計測対象外に逃がさない。実 IO（標準入出力・サブプロセス・端末・PTY・スレッド）は引数やジェネリックで注入し、本物の IO は合成ルート（ルートの `src/main.rs`）で束ねる。
  - 計測から外す必要がある item には、ファイル名の正規表現ではなく該当する module または function に `#[coverage(off)]` を付ける（外部 module ファイル全体を外す場合は inner attribute の `#![coverage(off)]`）。使用できるのは、テスト可能なロジックを抜いたあとの「実 IO そのもの」、または LLVM coverage が generic の単相化を重複計上する場合に限る。いずれも振る舞いを検証する fake / integration test を残し、除外理由を同じ変更に記録する。未テストの業務ロジック、到達しにくい error path、短期的な coverage 目標の回避には使わない。
  - `#[coverage(off)]` は nightly の `coverage_attribute` feature を必要とする。通常の build / test と coverage gate は、同じ nightly toolchain で実行する。
- 緊急時のフックスキップ: `LEFTHOOK=0 git commit ...` または `--no-verify`（原則使わない）。

## 変更箇所からの推奨テスト

開発中の fast feedback と commit 前の selected tests には `scripts/recommend-tests.sh [base]` を明示的に実行する。
`base` の既定値は `HEAD` で、`git diff` の変更 path、選定理由、近いテストコマンドを表示する。path とテストの
対応表は `scripts/recommend-tests.tsv` が正本である。

```bash
scripts/recommend-tests.sh origin/main
```

推奨された selected tests は PR 前の full gate の代替ではない。未知の path、空 diff、複数クレートにまたがる変更、
共有基盤の変更は fail-safe に `cargo test --workspace --quiet` を含める。コミット・push 前には、この節の出力にかかわらず
[品質チェック](#品質チェックリスク比例の-gate)の該当 gate を実行する。

## Git Hooks（lefthook）

| フック | 内容 |
|---|---|
| pre-commit | workspace root コミットの拒否（backstop） / ブランチ名チェック / staged な `.rs` を `cargo fmt` |
| commit-msg | Conventional Commits 形式チェック |
| pre-push | `cargo clippy --workspace --all-targets -- -D warnings` / テストカバレッジ 100% 確認（`cargo llvm-cov`。テスト実行を兼ねる。`*.rs` 差分が無い push は skip） |

### workspace root コミットの拒否（backstop）

pre-commit は、**リポジトリルートのチェックアウト（`.usagi/sessions/` 配下でない）で実装コミットしようとすると拒否**する。「変更は必ず session 内で行う」という運用を守るための安価な最終防壁で、拒否時は session を作成してその worktree でコミットするよう案内する。

- 判定はブランチ名チェックの免除と同じく「worktree パスが `.usagi/sessions/` 配下か」で行う。`.usagi/sessions/<name>/` 配下の worktree のコミットは通す。
- 誤検知を避けるため、対象は root に `.usagi/` を持つ usagi 管理ワークスペースに限る。usagi をライブラリとして使うだけの一般リポジトリの root コミットは妨げない。
- ローカル hook は迂回可能なため、[CI](#cigithub-actions) のブランチ保護と併せて多層で守る。
- 緊急脱出は従来どおり `LEFTHOOK=0 git commit ...` / `--no-verify`（原則使わない）。

## CI（GitHub Actions）

`main` への push / PR をトリガーに自動チェックが走る。

| ファイル | トリガー | 役割 |
|---|---|---|
| `.github/workflows/test.yml` | `main` への push / PR | fmt / clippy と full test（`--workspace`）を独立 job で並列実行（`ubuntu-latest`） |
| `.github/workflows/v1-test.yml` | `v1/**` を変更する push / PR | 退避された v1（リリースの出荷物）を `v1/Cargo.toml` を対象に fmt / clippy / full test で検証 |
| `.github/workflows/test-metrics.yml` | 毎週 / 手動 | nextest で full suite を retry なしで 3 回実行し、test ごとの JUnit、slow 上位、run-to-run variance を artifact 化（required gate ではない） |
| `.github/workflows/tui-e2e.yml` | `main` 向け PR / merge queue / 明示的手動実行 | 出荷物 v1 の実 PTY TUI E2E。PR / merge queue では `v1/Cargo.toml` の `[package].version` が base と異なる場合だけ実行し、通常 PR の重い test を回避する |
| `.github/workflows/release-build-check.yml` | `v1/Cargo.toml` / `v1/Cargo.lock` を変更する PR | リリースと同じ 4 プラットフォーム（Linux / macOS amd64・arm64 / Windows）で v1 を `cargo build --release` し、version 変更（＝タグが変わる PR）でリリースビルドが成功することをマージ前に検証 |
| `.github/workflows/coverage.yml` | PR | カバレッジ計測・PR コメント・100% 未満で失敗 |
| `.github/workflows/markdown-link-check.yml` | `.md` 変更を含む push / PR | Markdown のリンク切れ（相対リンク・アンカー・外部 URL）を [lychee](https://github.com/lycheeverse/lychee) で検証 |
| `.github/workflows/enforce-pr-base.yml` | PR | ベースブランチが `main` であることを強制 |

- リンクチェックの設定（リトライ・除外・アンカー検証）は `lychee.toml` に集約する。ファイル内の見出しアンカー（`#見出し`）も検証するため、目次リンク等が見出しと一致していないと失敗する。
- Rust の test / coverage workflow は PR または branch ごとに最新の実行だけを継続し、古い commit の実行をキャンセルする。
- TUI E2E の version 判定は checkout 済みの HEAD ではなく、イベントが渡す base SHA と head SHA のそれぞれから `[package].version` を読む。したがって、同じ `v1/Cargo.toml` を編集しても version が不変なら job は skip され、fork PR でも secrets や書き込み権限を必要としない。merge queue では合成 head と queue base を同じ方法で比較する。手動実行は input を明示して release candidate を再検証するときだけ実行する。

## リリース

リリースは **`v1/Cargo.toml` の `version` 変更を起点に自動化**されている。手動でタグを切る必要はない。
出荷するバイナリはまだ v1（`v1/` に退避された実装）であり、ルートの v2 workspace の version はリリースに
影響しない（退避時点の v1 の version を引き継いでおり、v2 として最初にリリースするときにリリース起点を
ルートへ切り替える）。

### 手順

1. リリースしたい変更を `main` にマージする。
2. `v1/Cargo.toml` の `version` を上げる PR を作成し `main` にマージする（`create-release-pr.yml` の手動実行でも作成できる）。
3. 以降は自動で進む:
   - `auto-release.yml` が `main` への `v1/Cargo.toml` 変更 push を検知し、version が前コミットから変わっていれば `v<version>` タグを対象にリリースを起動する。
   - reusable な `release.yml` が呼ばれ、4 プラットフォーム（Linux / macOS amd64・arm64 / Windows）で v1 のバイナリをビルドし、`v<version>` タグと GitHub Release を作成して成果物を添付する。

> version が変わらない push、または同名タグが既に存在する場合はスキップされる。

### ワークフロー構成

| ファイル | トリガー | 役割 |
|---|---|---|
| `.github/workflows/create-release-pr.yml` | 手動（`workflow_dispatch`） | 指定 version へ `v1/Cargo.toml` を更新するリリース PR を作成する |
| `.github/workflows/auto-release.yml` | `main` への `v1/Cargo.toml` 変更 push | version 変更を検知し `release.yml` を呼び出す |
| `.github/workflows/release.yml` | `v*` タグ push / `workflow_call` | リリースノート生成・v1 のビルド・GitHub Release 作成 |

`release.yml` は `v*` タグの手動 push でも従来どおり動作する（`workflow_call` は追加のトリガー）。
