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
- [`coverage(off)` 例外](#coverageoff-例外)
- [変更箇所からの推奨テスト](#変更箇所からの推奨テスト)
- [結合テストからの daemon 起動](#結合テストからの-daemon-起動)
  - [重い E2E の直列化](#重い-e2e-の直列化)
  - [daemon E2E の transient と product 失敗を混同しない](#daemon-e2e-の-transient-と-product-失敗を混同しない)
- [背景 worker を残したままテストを終えない](#背景-worker-を残したままテストを終えない)
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

外部依存は**必要になった時点で追加**する。version は
ルート `Cargo.toml` の `[workspace.dependencies]` で一元管理し、各クレートは
`<crate>.workspace = true` で参照する。

現在追加済みの外部依存は次のとおり。

| クレート | 使途 | 種別 |
|---|---|---|
| `chrono` | domain エンティティの時刻 | 本依存 |
| `serde` | エンティティ・インデックスの JSON (de)serialize derive | 本依存 |
| `uuid` | v2 resource incarnation の typed ID（UUIDv4）と durable operation ID（UUIDv7） | 本依存 |
| `serde_json` | `index.json` / `workspaces.json` / `daemon.json` の (de)serialize、`usagi-cli` の MCP サーバの stdio JSON-RPC、`usagi-daemon` の IPC メッセージの wire JSON | 本依存 |
| `toml` | `usagi-core` infrastructure による `.usagi/config.toml` の runtime/model allowlist と global/workspace `roles.toml` の解析 | 本依存 |
| `sha2` | issue / memory Markdown source set の deterministic fingerprint、build artifact / rollover operation identity、self-update の embedded installer identity | 本依存・build 依存 |
| `anyhow` | infrastructure（永続化ストア）と MCP store adapter のエラー伝播 | 本依存 |
| `fs2` | ストア、daemon current locator、合成ルートの daemon 単一インスタンスの cross-process ロック（`flock` 相当）と、結合テストの重い E2E 直列化ロック | 本依存 |
| `dirs` | 既定データディレクトリ（`~/.usagi`）の解決 | 本依存 |
| `rayon` | markdown ファイルの並列スキャン | 本依存 |
| `shell-words` | `usagi-core` の usecase（`workspace_guard`）による root モードの Bash command の字句分割 | 本依存 |
| `unicode-width` | 端末セルの表示桁数測定（CJK など全角の 2 桁計上）。`usagi-core` の VT parser（`usecase::vt_screen`）と `usagi-tui` の描画が使う | 本依存 |
| `clap` | 入口面 CLI の引数解析（コマンドツリー定義） | 本依存 |
| `clap_complete` | `usagi completion <shell>` のシェル補完スクリプト生成 | 本依存 |
| `crossterm` | 対話 TUI の実端末バックエンド（raw mode・代替スクリーン・キー/リサイズイベント） | 本依存 |
| `portable-pty` | `usagi-daemon` の infrastructure（`pty`）による PTY の確保、子プロセス起動、入出力・resize・wait | 本依存 |
| `libc` | `usagi-core` infrastructure の atomic temp file 検証（Unix の no-follow・所有者・link identity）と合成ルートでの daemon process-start identity 観測・exact-owner signal | 本依存 |
| `signal-hook` | 合成ルートで daemon の SIGINT / SIGTERM handler と同期 wait を worker spawn 前に準備する | 本依存 |
| `tempfile` | ストアのユニットテスト用の一時ディレクトリ | dev |

`usagi-core` の `domain/`（`Workspace` / `Issue` / `Memory` / `DaemonRecord` / `Recent` / typed ID …）は
`chrono` / `serde` / `uuid` だけを使う。`serde_json` / `anyhow` / `fs2` / `dirs` / `rayon` は
`infrastructure/`（永続化）が使い、`serde_json` は加えて `usagi-cli` の MCP サーバ（stdio
JSON-RPC）と `usagi-daemon` の IPC メッセージ (de)serialize でも使う。`toml` は `usagi-core` の infrastructure
（runtime/model と role catalog reader）、`shell-words` は同クレートの usecase が使い、いずれも domain には持ち込まない。
`unicode-width` は
`usagi-core` の usecase 層（VT parser `vt_screen`）と `usagi-tui` の描画が使う（domain の
`chrono` / `serde` / `uuid` 規則は不変で、`unicode-width` は domain には持ち込まない）。
`clap` / `clap_complete` は `usagi-cli` が使う。
`sha2` は合成ルートの `build.rs` が source / build configuration identity、runtime が workspace・executable 別の bootstrap broker endpoint、IPC contract が rollover operation ID を
作るほか、`usagi-cli` が配布 binary に同梱した self-update installer の identity 検証にも使う。
`chrono` / `anyhow` は `usagi-cli` の MCP store adapter が実時計の束縛と core usecase の
エラー変換にも使う。`fs2` は `usagi-daemon` の current locator publish / retire と、ルート結合テストの[重い E2E の直列化](#重い-e2e-の直列化)も直列化する。
`portable-pty` は `usagi-daemon` の infrastructure に閉じ込め、daemon の usecase 層は PTY ポートを介して使う。
`libc` は `usagi-core` の infrastructure に閉じた atomic temp file の Unix 安全性検証にも使う。
`crossterm`（実端末 IO）・`libc`（daemon の process-start identity 観測と fenced signal）・`signal-hook`（daemon shutdown signal）・`fs2`（daemon 単一インスタンス
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
- 本文の `Internal-Issue` は、内部 issue を実装する変更では `#<number>`、内部 issue が無い変更では `none` とする。
  番号を記した PR は対応する `.usagi/issues/<number>-*.md` を同じ差分で `done` にし、CI がこの同期を検証する。
- ベースブランチは `main`。[CI](#cigithub-actions) が強制する。
- **PR は Draft で開き、[CI](#cigithub-actions) の必須チェック（fmt / clippy / full test / coverage 100%、該当時は Markdown link check）が green になってから Ready for review にする**。ローカル push では重い full gate を走らせないため（[Git Hooks](#git-hookslefthook)）、最終的な full gate の green は CI で確認する。CI が落ちたら Draft のまま修正して push し直す。

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

- **層をまたいで書かない**。v2 の実装が増えて仕様ドキュメントを追加するときも、1 つの事実の置き場所を
  1 か所に保つ。

### 構造

- **1 ファイル = 1 トピック**。番号付きファイル（`01-` …）で構成する（[目次](README.md) 参照）。
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

検証 gate は「編集中の fast loop」「commit 前」「push 前（ローカル）」「PR・CI（最終 full gate）」に分ける。
この節が、開発者・AI エージェント双方の品質チェックの正本である。workspace 構成のため、test / clippy / check には
**必ず `--workspace` を付ける**（ルートで実行するとルートパッケージしか対象にならない）。

**Rust の重い full gate（workspace clippy / full test / coverage 100%）はローカル push では強制しない**。
最終的な full gate は PR CI に一本化し、ローカルは開発中の fast feedback と commit 前の selected tests に軽く保つ。
[Git Hooks](#git-hookslefthook) の pre-push はこの重い gate を持たない。

| 段階 | 必須 gate | コマンド |
|---|---|---|
| 編集中 | フォーマット差分の確認 / コンパイル確認 / 変更 crate・module の test | `cargo fmt --all -- --check` / `cargo check --workspace --all-targets` / 変更箇所に対応する `cargo test -p <crate>` |
| commit 前 | Lint / risk-based selected tests | `cargo clippy --workspace --all-targets -- -D warnings` / `scripts/recommend-tests.sh origin/main` が示す test（または同等以上の理由付き selected tests） |
| push 前（ローカル） | Markdown link check（Markdown 差分あり） | `lychee --config lychee.toml --no-progress '*.md' 'document/**/*.md' '.agents/**/*.md' '.github/**/*.md'` |
| PR・CI（最終 full gate） | 対象差分の fmt / clippy / full test / coverage 100% / Markdown link check と、全 PR での aggregate context 報告 | `.github/workflows/test.yml` が fmt / clippy / `cargo test --workspace --quiet`、`.github/workflows/coverage.yml` が coverage 100%、`.github/workflows/markdown-link-check.yml` が Markdown link check を実行する。対象外差分は重い job を省略し、stable aggregate は success を報告する |

PR は Draft で開き、上表の CI 必須チェックが green になってから Ready for review にする（[プルリクエスト](#プルリクエスト)）。
最終的な full gate（clippy / full test / coverage 100%）の green は CI で確認するのが正であり、ローカルで先取りして
確認したい場合は次の経路を使ってよい（任意）。`cargo llvm-cov` はテスト実行を兼ねるため、この経路では同じ差分に対して
`cargo test --workspace --quiet` を重複実行しなくてよい。

```bash
. ./scripts/coverage.sh
coverage_enforce
```

docs-only（Rust 差分なし）は Rust gate（`cargo check` / `cargo clippy` / `cargo test` / coverage）を省略できる。ただし
Markdown 差分を含むため、Markdown link check は必須である。

CI で full test / coverage gate が必須となる条件は次のとおり（この gate は CI が強制する）。

- Rust 差分（`*.rs`、`Cargo.toml`、`Cargo.lock`、Rust の build / test / coverage に影響する `scripts/`・`.github/workflows/`・hook）を含む。
- docs-only を除き、`scripts/recommend-tests.sh` が fail-safe として `cargo test --workspace --quiet` を推奨する（未知の path、空 diff、複数クレートにまたがる変更、共有基盤の変更など）。
- 変更がクレート境界・層境界、永続化、process / PTY / terminal IO、設定解決、テスト基盤、coverage 除外、CI / hook の gate に影響する。
- selected tests で対象リスクを説明できない、または直接 consumer を特定できない。

- テストカバレッジ 100% を維持する（CI でチェック）。
  - **依存を注入してテスト可能にする**。「テストできないから」とロジックを計測対象外に逃がさない。実 IO（標準入出力・サブプロセス・端末・PTY・スレッド）は引数やジェネリックで注入し、本物の IO は合成ルート（ルートの `src/main.rs`）で束ねる。
  - 計測から外す必要がある item には、ファイル名の正規表現ではなく該当する module または function に `#[coverage(off)]` を付ける（外部 module ファイル全体を外す場合は inner attribute の `#![coverage(off)]`）。使用できるのは、テスト可能なロジックを抜いたあとの「実 IO そのもの」、または LLVM coverage が generic の単相化を重複計上する場合に限る。いずれも振る舞いを検証する fake / integration test を残し、除外理由を同じ変更に記録する。未テストの業務ロジック、到達しにくい error path、短期的な coverage 目標の回避には使わない。
  - `#[coverage(off)]` は nightly の `coverage_attribute` feature を必要とする。通常の build / test と coverage gate は、同じ nightly toolchain で実行する。
  - **その nightly は `rust-toolchain.toml` で日付 pin する**。`channel = "nightly"` のままだと CI が毎回その日の nightly を取り、新しく安定化した lint が既存コードで一斉に発火して無関係な PR まで Rust lint で落ちる。toolchain の更新は「pin を上げる PR」で意図的に行い、その PR で新 lint の対応もまとめる。CI の workflow は `dtolnay/rust-toolchain@nightly` で component を入れるが、その対象は日付なしの `nightly` なので、pin した toolchain が使う component（`llvm-tools-preview` / `rustfmt` / `clippy`）は `rust-toolchain.toml` の `components` が正本である。
- 緊急時のフックスキップ: `LEFTHOOK=0 git commit ...` または `--no-verify`（原則使わない）。

## `coverage(off)` 例外

この節が v2 の coverage exclusion policy の正本である。許可理由は、テスト可能な判断を分離したあとの
`real_io`（OS・端末・PTY・process そのもの）、production の依存を束ねるだけの `composition`、LLVM が
generic 単相化を重複計上する `generic_monomorphization` の 3 種類に限る。reducer、parser、validation、
reconcile、error mapping は許可理由にならない。許可する item にも、同じ振る舞いを port/fake で検証する unit test
または本物の境界を検証する integration test が必要である。

例外はルートの `coverage-off-allowlist.json` に `path` / `symbol` / `occurrence` / `reason` / `owner` /
`expires` / `tests` を登録するか、属性と同じ行へ次の機械可読コメントを書く。

```rust
#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=pty_integration
fn read_real_pty() { /* OS call only */ }
```

期限は最長でも次の返済 review までとし、無期限の例外を置かない。既存 exclusion は `migration_debt` として
凍結されており、これは新規に選べる許可理由ではない。返済時はテストを追加して属性と registry entry を同じ変更で
削除するか、上記 3 理由のいずれかを証拠テストとともに登録し直す。追加・削除後は `ruby scripts/coverage-off-lint.rb`
を実行する。source だけの追加、registry だけ残る stale symbol、重複、理由・owner・期限・テスト証跡の欠落、期限切れ、
許可外理由に加え、[`coverage-off-budget.json`](../coverage-off-budget.json) の owner / path 別 inventory と一致しない
件数変更は CI を失敗させる。budget 更新は増減を review 上で明示するだけで、許可理由の適合確認を代替しない。
現行 debt の領域別 inventory と返済順序は
[8. coverage exclusion inventory](08-coverage.md) を参照する。

## 変更箇所からの推奨テスト

開発中の fast feedback と commit 前の selected tests には `scripts/recommend-tests.sh [base]` を明示的に実行する。
`base` の既定値は `HEAD` で、`git diff` の変更 path、選定理由、近いテストコマンドを表示する。path とテストの
対応表は `scripts/recommend-tests.tsv` が正本である。

v2 の主要な対応は次のとおり。crate の integration test は package 全体ではなく、そのファイル名と同じ
`--test <target>` を選ぶ。root runtime は合成ルートの bin target、root integration test は同名 test target を選ぶ。

| 変更 path | 推奨 test |
|---|---|
| `crates/core/src/*` | `cargo test -p usagi-core` |
| `crates/daemon/src/*` | `cargo test -p usagi-daemon` |
| `crates/tui/src/*` | `cargo test -p usagi-tui` |
| `crates/cli/src/*` | `cargo test -p usagi-cli` |
| `crates/{core,daemon,tui}/tests/<target>.rs` | `cargo test -p <package> --test <target>` |
| `src/runtime/*` / `src/tui_input.rs` | `cargo test -p usagi --bin usagi` |
| `tests/<target>.rs` | `cargo test -p usagi --test <target>` |

```bash
scripts/recommend-tests.sh origin/main
```

推奨された selected tests は CI の full gate の代替ではない。未知の path、空 diff、複数クレートにまたがる変更、
共有基盤の変更は fail-safe に `cargo test --workspace --quiet` を含め、出力の `Fallback reasons` にその根拠を表示する。
共有基盤には全階層の Cargo manifest / lockfile、`crates/core/src/infrastructure/ipc/*` の共有 IPC protocol、
crate/root の test support、build/test script・hook・CI workflow を含む。既知 path でも複数の責務領域にまたがれば full fallback
とする。同じ領域の複数 path が同じ command を選んだ場合は 1 件に正規化する。

対応表を編集したときは fixture test に加えて `--validate-map` を実行する。各 rule の witness が先行 rule に shadow されず、
推奨 command の v2 package と test/bin target が Cargo metadata に実在することを検証する。

```bash
scripts/recommend-tests.sh --validate-map
bash scripts/tests/recommend-tests.sh
```

コミット・push 前には、この節の出力にかかわらず
[品質チェック](#品質チェックリスク比例の-gate)の該当 gate（commit 前の Lint / selected tests、push 前の Markdown link check）を実行し、
最終的な full gate は PR CI の green で確認する。

## 結合テストからの daemon 起動

ルートの結合テスト（`tests/*.rs`）が `usagi` プロセスを起動するときは、**必ず `tests/support/daemon.rs` の
command builder 経由**で起動する。直接 `Command::new(env!("CARGO_BIN_EXE_usagi"))` を書かない。

daemon の workspace root は**起動時 cwd** で決まるため（[5. daemon](05-daemon.md#daemon-process-lifecycle)）、cwd を
指定せずに起動したテストは、開発者のチェックアウト（セッション worktree）を権威として掴んだ daemon を作ってしまう。
その daemon は worktree 内の実行ファイルを掴んだまま残留し、`session remove` の worktree 削除を止める。helper は
次の 3 点を 1 か所で保証する。

| 保証 | 内容 |
|---|---|
| cwd の隔離 | 起動ごとに fixture workspace を `.current_dir()` に設定し、cwd がチェックアウトの内側でないことを assert する |
| workspace root の回帰検出 | fixture の teardown で、起動した daemon が adopt した workspace（`daemon/w/<digest>/root.json`）がすべて fixture であることを assert する |
| exact reap | teardown で graceful stop を試み、残った場合だけ `daemon.json` の pid + process-start identity が一致する incarnation へ SIGTERM → SIGKILL と段階的に落とす |

`daemon serve` の直接起動だけでなく、`daemon start` / `daemon restart` と client bootstrap（`session ...` /
`mcp` / TUI）による間接起動も同じ経路に載せる。自プロセス上に fake daemon を立てるテストの record は reap 対象外に
なる（自分自身を撃たない）。

### 重い E2E の直列化

shipping binary・daemon・fixture provider・実 PTY を同時に走らせる E2E は CPU を占有する。並行させると frame 待ちや
readiness 待ちが product の失敗ではなく CPU 競合による timeout になり、無関係な変更の PR を落とす偽陽性を生む。
そこで重い E2E は、**チェックアウト単位の 1 本の直列列**に載せる。列は `tests/support/daemon.rs` の
`heavy_e2e_lock()` が持つ。新しい実 PTY / 実 daemon E2E を追加するときは、同じ関数を test の先頭で呼ぶ。

| 競合の出どころ | この列が覆うか |
|---|---|
| 同じ test binary 内の別 test（libtest の thread 並行） | 覆う（プロセス内 `Mutex`） |
| 同じ `cargo test --workspace` 実行の別 test binary | 競合しない。cargo は test binary を**直列に**実行する（full run 468 サンプルで同時実行数は常に 1） |
| 同じチェックアウトの別 cargo 実行（`cargo test` と `cargo llvm-cov` の同時実行など） | 覆う（lock file を `flock`） |
| 別チェックアウト・別ユーザー・マシン上の他プロセスの負荷 | 覆わない（環境側の条件として残る） |

lock file は temp directory 上に置き、チェックアウト path の digest で名付ける。`target/` の下に置かない:
`cargo test` と `cargo llvm-cov` は別の target directory を使うため、target 配下だと**同じ tree の 2 つの実行が
別々のロックを取ってしまい**、直列化したい当の組み合わせだけが漏れる。

ロック取得は上限付きで、超えたら lock file を名指しして失敗する（先行 process の hang を無期限の停止にしない）。

### daemon E2E の transient と product 失敗を混同しない

実 daemon を相手にする E2E は、**socket が閉じただけの transient**（endpoint 公開中、listener の retire、
pre-handshake 上限による accept 直後の close）と、**product の失敗**（daemon の panic・異常終了・typed な拒否）を
区別する。前者は production の client も新しい接続で retry する（`usagi_core::usecase::client` の `PolicyClient` は
`is_transport_failure` を retry 対象にする）ため、テストの readiness 待ちでも deadline 内で retry する。

区別を成立させるために、次の 2 つを守る。

- daemon が close した接続を retry で吸収してよいのは、**同じ producer `OperationId` を再送する**場合か、
  request が read-only の場合に限る。新しい operation を作り直して再送しない（二重実行を pass に読み替えてしまう）。
- retry のたびに daemon の error log（`<data dir>/logs/`）を見て、`daemon panicked` が記録されていたら即座に失敗する。
  daemon は process 全体の panic hook で全 thread の panic をここへ書き、stderr は production と同じく `/dev/null` へ
  捨てるため、**この log が「transient か product 失敗か」を判定できる唯一の証拠**である。失敗時のメッセージにも
  この log を添える。

## 背景 worker を残したままテストを終えない

product の worker thread（restore job・resident lane など）を動かすテストは、**その worker が仕事を終えてから終了する**。
実行中の worker を残して test が返ると、worker は残りの suite の間も product 関数の coverage counter を書き続け、
harness が profile を書き出す瞬間と競合する。counter の増分はアトミックでなく、region の一部は差分（counter 式）で
表現されるため、この競合は**別のテストが確実に踏んでいる行を「未達」として報告させる**。coverage gate は全 PR で
共有されるため、これは無関係な変更の PR を落とす。

- 待つ条件は fake 側の観測に載せる（呼び出し回数が数 tick 進まない、完了通知を受け取る、など）。固定 sleep で
  代用しない。
- テストの決定性そのものも同じ形で作る。frame skip や retry のような**タイミングで決まる事象は、観測できるまで
  loop を駆動し、観測できない run は上限で失敗させる**（skip が起きなかった run を pass と読み替えない）。
  `usagi-tui` の `presentation::tests::a_skipped_tick_still_admits_the_restore_retry` がこの形で、restore retry が
  frame を skip した tick で admit されるのを観測し、その job の inventory 呼び出しが静まってから quit する。
- **待ちの needle は、待っている状態を実際に区別できるものにする**。入力を送ってから次の入力を送る E2E で、
  遷移の前後どちらの画面にも出る文字列を待つと、その待ちは即座に満たされて次の入力が前の状態へ入る。
  overlay を閉じてから次のキーを送るなら、閉じたことを**不在**で観測する（`cli_tui_pty` の
  `wait_for_screen_absent_since`）。負荷が高い run だけ落ちる E2E は、まずこの形を疑う。
- **「遅い」と「落ちた」を product の報告で切り分ける**。TUI は lane が reconnect / resync 中に受けた
  keystroke を捨て、`… keystroke not delivered` と報告する。捨てられた入力は待っても現れないので、
  deadline を伸ばしても直らない。報告を観測して**打ち直す**（`cli_tui_pty` の
  `send_line_until_delivered`）。単に遅いだけの run では product は何も報告しないので、そのまま待つ。
  これは `coverage` job で特に重要である。`cargo llvm-cov` は instrumentation 付きで全 test を走らせる分
  構造的に遅く、こうした窓が広がる。**上限を実時間から「出力の停滞」へ変える形は使えない**。TUI は
  待っている間も再描画し続けるため停滞せず、上限が永久に来なくなる。
- **失敗時の診断は product の文言に固定リストで依存しない**。文言が変われば、まさにその原因で落ちた run が
  「該当なし」と表示され、原因を取り違える。安定した語尾や描画行そのものを出す。

## Git Hooks（lefthook）

| フック | 内容 |
|---|---|
| pre-commit | workspace root コミットの拒否（backstop） / ブランチ名チェック / staged な `.rs` を `cargo fmt` |
| commit-msg | Conventional Commits 形式チェック |

**pre-push フックは持たない**。以前はここで `cargo clippy --workspace` とカバレッジ 100%（`cargo llvm-cov`）を実行していたが、
push のたびにローカルで重い full gate が走り開発のリズムを損なっていた。clippy / full test / coverage 100% の最終 gate は
[CI](#cigithub-actions)（`test.yml` / `coverage.yml`）に一本化し、ローカルは commit までの軽い gate に保つ。
最終的な full gate の green は、Draft PR の [CI](#cigithub-actions) が緑になったことで確認する（[プルリクエスト](#プルリクエスト)）。

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
| `.github/workflows/test.yml` | `main` への push / PR | Rust gate 対象差分では fmt / clippy と full test（`--workspace`）を独立 job で並列実行し、全差分で `test` / `full-test` aggregate を報告 |
| `.github/workflows/test-metrics.yml` | 毎週 / 手動 | nextest で full suite を retry なしで 3 回実行し、test ごとの JUnit、slow 上位、run-to-run variance を artifact 化（required gate ではない） |
| `.github/workflows/tui-e2e.yml` | `main` 向け PR / merge queue / 明示的手動実行 | 現行パッケージの実 PTY TUI E2E。PR / merge queue ではルート `Cargo.toml` の `[package].version` が base と異なる場合だけ実行する |
| `.github/workflows/release-build-check.yml` | ルート `Cargo.toml` / `Cargo.lock`、またはリリース経路の workflow / `rust-toolchain.toml` を変更する PR | リリースと同じ 3 プラットフォーム・同じ `--features production` で `cargo build --release` し、リリースビルドが成功することをマージ前に検証する。host target では installer の version 出力契約も検証する。workflow 自身も trigger に含めるのは、リリース経路を変更する PR では version が動かず、version だけを trigger にすると経路の変更が無検証でマージされるためである |
| `.github/workflows/coverage.yml` | PR | Rust gate 対象差分では `coverage(off)` registry lint、カバレッジ計測・未達レポート（PR コメント + Job Summary）・100% 未満で失敗し、全差分で `coverage` aggregate を報告 |
| `.github/workflows/markdown-link-check.yml` | `main` への push / PR | Markdown 対象差分ではリンク切れ（相対リンク・アンカー・外部 URL）を [lychee](https://github.com/lycheeverse/lychee) で検証し、全差分で `markdown-link-check` aggregate を報告 |
| `.github/workflows/enforce-pr-base.yml` | PR | ベースブランチが `main` であることを強制 |
| `.github/workflows/security-audit.yml` | 毎週 / 手動 | `Cargo.lock` を RustSec advisory database と照合する。PR / `main` push では `test.yml` の policy check として同じ audit を実行し、required `test` aggregate が結果を伝播する |

- Rust 依存は Dependabot が毎週更新 PR を作り、GitHub Actions の参照も同じ周期で更新する。RustSec advisory を一時的に
  除外するときは `.github/security-audit-exceptions.json` に advisory ID、GitHub handle の `owner`、ISO 形式の `expires`、
  具体的な `rationale` を登録する。`expires` は検証日から 90 日以内とし、期限切れ・必須項目欠落・未知フィールド・重複は
  audit 前の checker が拒否する。更新には改めて owner と rationale のレビューを必要とし、恒久的な除外は認めない。
- リンクチェックの設定（リトライ・除外・アンカー検証）は `lychee.toml` に集約する。ファイル内の見出しアンカー（`#見出し`）も検証するため、目次リンク等が見出しと一致していないと失敗する。
- `test.yml` は `scripts/ci/root-readme.sh` でルート `README.md` の最低限の contract（`# usagi` 見出し・`document/` の正本へのリンク・truncation 検出のための本文行数）を検証する。リンクチェックはリンクが 0 本になった README を通してしまい、実際にルート README が 1 行へ破壊されたまま `main` に残った事故があるため、この checker が独立した gate として必要である。checker 自体は `scripts/tests/root-readme.sh` の fixture test で検証する。
- Rust の test / coverage workflow は PR または branch ごとに最新の実行だけを継続し、古い commit の実行をキャンセルする。
- required status check の正本は `.github/required-contexts.json` である。ruleset `17627257` は `test`、
  `enforce-base-main`、`full-test`、`coverage`、`markdown-link-check` を GitHub Actions（integration ID
  `15368`）の required context として持つ。各 workflow は path filter をイベントに置かず、軽量な差分判定後に対象の重い job だけを
  実行する。aggregate job は `if: always()` で判定 job と実行 job の結果を検査するため、Rust、Markdown-only、既知の静的 asset の
  どの PR でも同じ context 名を報告し、判定失敗や対象 job 失敗を success へ変換しない。未知 path は fail-safe で Rust gate 対象とする。
- `scripts/ci/required-contexts.sh audit-workflows` は contract の workflow / job / context 名を照合し、rename drift を検出する。
  `scripts/tests/required-contexts.sh` は Rust、Markdown-only、無関係 path の分類と aggregate、ruleset payload の fixture を検証する。
  workflow 名や job 名を変更するときは、ruleset を先に変更して存在しない context を required にせず、同じ変更で contract と fixture を
  更新する。
- ruleset 更新では、更新直前の GET response を保存し、`scripts/ci/required-contexts.sh prepare-ruleset` で update payload と
  rollback payload を生成する。PUT 後は GET response へ `verify-ruleset` を実行する。失敗時は保存した rollback payload を同じ ruleset へ
  PUT する。bypass policy は RepositoryRole ID `5`（admin）の `always` だけを許可し、通常 merge は required context と 1 approval を
  必須にする。bypass や admin merge は障害復旧など明示的に承認された操作に限り、通常の workflow rollout には使用しない。
- `coverage.yml` は 100% 計測の前に `scripts/coverage-off-lint.rb` を実行する。lint 自体は `scripts/tests/coverage-off-lint.sh` の fixture（許可 IO、禁止 reducer、理由欠落、stale、追加、削除、期限切れ）で検証し、`test.yml` でも実行する。
- カバレッジ未達（100% 未満）のとき、`coverage.yml` は `cargo llvm-cov report --json` から**未達ファイルと未達関数**（ファイル path・関数名・宣言行・関数率/行率・不足量・未達行レンジ）のレポートを生成し、PR コメント（同一リポジトリ PR。`marocchino/sticky-pull-request-comment` の header + recreate で再実行時も 1 件に更新）と Job Summary の両方へ出す。Job Summary は権限不要のため fork PR でも一覧が見え、コメント投稿は `continue-on-error` で **coverage gate の合否（exit code）から独立**させる。関数カバレッジは JSON summary（generic の単相化をマージした集計＝gate と一致。lcov の per-monomorphization な `FN/FNDA` を数えると gate と食い違う）を使い、関数名は `c++filt`（binutils。Rust v0 を demangle）で可読化する。出力はファイル/関数/行レンジの上限で切り詰め、超過分は明示する。レポート生成は `scripts/coverage-report-comment.rb`（Ruby, stdlib のみ）に抽出し、`scripts/tests/coverage-report-comment.sh` の fixture test（`test.yml` の script-tests job で実行）で固定する。閾値・対象パッケージ選択の SSoT は `scripts/coverage.sh`。
- TUI E2E の version 判定は checkout 済みの HEAD ではなく、イベントが渡す base SHA と head SHA のそれぞれからルート `[package].version` を読む。version が不変なら job は skip され、fork PR でも secrets や書き込み権限を必要としない。merge queue では合成 head と queue base を同じ方法で比較する。手動実行は input を明示して release candidate を再検証するときだけ実行する。

## リリース

リリースは **ルート `Cargo.toml` の `version` 変更を起点に自動化**されている。手動でタグを切る必要はない。
出荷するバイナリは**ルートパッケージ**である。

### 出荷 artifact の要件

| 要件 | 内容 |
|---|---|
| feature | `--features production` を付ける。この feature が無い artifact は `USAGI_RUNTIME_MODE` 未指定時に local を既定にするため、利用者のデータが `~/.usagi/local/` に入る（正本は [5. daemon#artifact の既定 mode](05-daemon.md#artifact-の既定-mode)） |
| toolchain | `rust-toolchain.toml` が pin した nightly。cross target は `rustup target add` で pin した toolchain へ入れる（`dtolnay/rust-toolchain` の `targets:` は日付なし nightly に入るため使わない） |
| 対象 platform | Linux amd64 / macOS amd64 / macOS arm64 の 3 つ。installer（`scripts/install.sh`）の `platform_asset` は darwin / linux だけを受け付け、それ以外は fail する。加えて v2 は Unix domain socket の IPC と Unix 専用の process / permission API に依存するため Windows ではコンパイルできない |
| archive | `usagi-<os>-<arch>.tar.gz`。中身は唯一の top-level entry `usagi`（installer の `verify_archive` が要求する） |
| verification artifact | 各 archive と同名の `.sha256` と `.version`。installer は両方を必須とし、存在しない旧 release へ無検証 fallback しない |
| version 出力 | `usagi <version>`（installer の `read_version` が要求する。`release-build-check.yml` が host target でこの契約を検証する） |

### 手順

1. リリースしたい変更を `main` にマージする。
2. ルート `Cargo.toml` の `version` を上げる PR を作成し `main` にマージする（`create-release-pr.yml` の手動実行でも作成できる）。
3. 以降は自動で進む:
   - `auto-release.yml` が `main` へのルート `Cargo.toml` 変更 push を検知し、version が前コミットから変わっていれば `v<version>` タグを対象にリリースを起動する。
   - reusable な `release.yml` が呼ばれ、上表の要件で 3 プラットフォームのバイナリをビルドし、`v<version>` タグと GitHub Release を作成して成果物を添付する。

> version が変わらない push、または同名タグが既に存在する場合はスキップされる。

### ワークフロー構成

| ファイル | トリガー | 役割 |
|---|---|---|
| `.github/workflows/create-release-pr.yml` | 手動（`workflow_dispatch`） | 指定 version へルート `Cargo.toml` を更新するリリース PR を作成する |
| `.github/workflows/auto-release.yml` | `main` へのルート `Cargo.toml` 変更 push | version 変更を検知し `release.yml` を呼び出す |
| `.github/workflows/release.yml` | `v*` タグ push / `workflow_call` | リリースノート生成・v2 のビルド（`--features production`）・SHA-256 / version artifact 生成・GitHub Release 作成 |

`release.yml` は `v*` タグの手動 push でも従来どおり動作する（`workflow_call` は追加のトリガー）。
