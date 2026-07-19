---
number: 380
title: ci(coverage): 失敗時に PR へ未達ファイル+関数一覧を安全・可読に投稿する（テスト可能な生成スクリプトに抽出）
status: todo
priority: medium
labels: [ci, coverage, dx]
dependson: []
related: []
created_at: 2026-07-19T22:53:57.627451+00:00
updated_at: 2026-07-19T22:53:57.627451+00:00
---

## 背景 / 目的

coverage CI（`.github/workflows/coverage.yml`）は現在、カバレッジ未達（Lines/Functions < 100%）のとき
PR に「100% に届いていないファイル」の一覧コメントを投稿するが、次の不足がある。ユーザー要求は
「coverage CI が失敗したとき、対象 PR に **未達のファイルと関数** を安全で読みやすく表示する」こと。

### 現状（調査結果）

`coverage.yml` の流れ:

1. `cargo llvm-cov --workspace --lcov --output-path lcov.info` — `lcov.info` を生成（テスト/ビルド失敗時のみ fail。カバレッジ低下では fail しない）。
2. **Generate coverage summary**（`if: success()`）— `coverage_report --summary-only` の**テキスト表**を awk で解析し、Lines が `100.00%` でないファイルを `File / Regions / Functions / Lines(+信号機emoji)` の行にして `coverage-comment.md` を生成。
3. **Comment coverage on PR**（`if: success()`）— `marocchino/sticky-pull-request-comment@v2`、`header: coverage`, `recreate: true`（再実行での重複は防げている）。
4. **Enforce 100% coverage**（暗黙の `if: success()`）— `coverage_report --fail-under-lines 100 --fail-under-functions 100` で job の exit code を決める。

閾値・対象パッケージ選択は SSoT の `scripts/coverage.sh`（`COVERAGE_MIN=100`、`coverage_report()` は `-p 'usagi*'`）。

### ギャップ

1. **関数名が出ない**。ファイル単位の率のみで、どの**関数**が未達かが分からない（`--summary-only` のテキスト表に関数名は無い）。要求は未達関数名・必要なら行率/関数率・不足量。
2. **fork / 権限で安全でない**。`permissions: pull-requests: write` は fork PR では read-only に降格される。その状態で marocchino のコメント step が 403 で**失敗**する。コメント step は enforce の**前**にあり暗黙の `success()` 連鎖のため、コメント失敗が job を落とし enforce を skip する → **fork PR ではカバレッジ 100% でも coverage CI が落ち、かつコメントも出せない**。gate の exit code はコメント成否から独立させ、fork でも一覧は見えるべき。
3. **失敗時の表示が脆い / fork 用フォールバックが無い**。今は「コメントが enforce より前」という順序に依存して偶然機能しているだけで、`$GITHUB_STEP_SUMMARY`（権限不要・fork でも必ず出る Job/Check Summary）への出力が無い。
4. **出力上限が無い**。大きな退行で数百ファイル/関数が並ぶと巨大コメントになる。
5. **生成ロジックが YAML 内の inline awk でテスト不能**。本リポジトリの慣行はテスト可能な抽出スクリプト（`scripts/*.rb|sh` ＋ `scripts/tests/*.sh` を `test.yml` の script-tests job で実行、例: `summarize-nextest-junit.rb` / `scripts/tests/summarize-nextest-junit.sh`）。要求も fixture/unit/integration 検証を求めている。

## スコープ / やること

### 1. 生成ロジックを**テスト可能なスクリプト**に抽出

`scripts/coverage-report-comment.rb`（**Ruby, stdlib のみ**。`summarize-nextest-junit.rb` に倣う。
※ **Rust では書かない** — 100% Rust coverage gate が「その gate を報告するツール自体」を計測する自己参照を避けるため。CI には既に Ruby がある）。

- 入力: 既に生成済みの **`lcov.info`**（＋閾値・上限を env/引数で）。出力: Markdown を stdout へ。
- lcov を解析: `SF:<src>` ごとに `FN:<line>,<name>` と `FNDA:<count>,<name>`（**count=0 が未達関数**）、`DA:<line>,<count>`（**count=0 が未達行**）、および `FNF/FNH`・`LF/LH`（率・不足量算出用）を集計する。
  - 実装前に `lcov.info` に FN/FNDA が含まれること・関数名が demangle 済みであることを確認する（cargo-llvm-cov の `--lcov` は既定で FN/FNDA/FNF/FNH を出力し demangle 済み）。関数名が不十分なら `cargo llvm-cov report --json` へのフォールバックを検討。
- 未達ファイル（lines か functions が 100% 未満）ごとに: **ファイル path** / 関数率・行率 / **不足量**（未達関数数 = `FNF-FNH`、未達行数 = `LF-LH`）と、**上限付き**の未達関数名（＋行番号）一覧を出す。行のみ未達（関数は全 hit）でも行不足を示す。
- **上限**: `MAX_FILES`（例 20）・`MAX_FUNCS_PER_FILE`（例 10）。超過時は「…ほか N 件」を付記して切り詰める（切り詰めを黙って隠さない）。Markdown 表を壊さないよう関数名の `` ` `` / `|` をエスケープする。
- 先頭に「現在の合計カバレッジ + 閾値に対する PASS/FAIL」を出す。全ファイル 100% のときは既存の祝いメッセージ（絵文字・トーン）を保つ。

### 2. `coverage.yml` を安全・確実に組み替える

- `lcov.info` 生成は維持。
- 新スクリプトで `lcov.info` からレポートを生成し、**`coverage-comment.md` と `$GITHUB_STEP_SUMMARY` の両方**へ書く（Summary は権限不要・fork でも必ず表示）。この step は `if: always()` かつ `lcov.info` が存在するときのみ（compile/test 失敗で lcov 未生成なら skip。その場合は test step が既に job を落としている）。
- sticky PR コメント（`header: coverage`, `recreate: true`＝重複防止は維持）は **job を落とさない**: `continue-on-error: true`、かつ同一リポジトリ PR に限定（`if: github.event.pull_request.head.repo.full_name == github.repository`）して fork PR は Job Summary にフォールバック。いずれにせよ gate はコメント成否に依存させない。
- **enforce step を exit code の唯一の決定者**とし、レポート生成の後に `if: always()`（lcov 存在時）で走らせる。これで「レポートを出す → <100% なら job を落とす」を保証し、**失敗時を確実に含める**を満たす。
- `permissions`（contents: read / pull-requests: write）は同一リポジトリのコメントに必要なので維持。fork では Job Summary へ degrade する旨をコメント/docs に明記。

### 3. テスト（`scripts/tests/coverage-report-comment.sh`、`test.yml` の script-tests job に追加）

fixture `lcov.info` で少なくとも次を固定する:

- 未達関数（`FNDA:0`）を持つファイル → 関数名＋行番号が出る／不足量が正しい。
- 関数は全 hit だが未達行（`DA:0`）を持つファイル → 関数 100% でも行不足が出る。
- 上限超過 → 「…ほか N 件」が出て一覧が切り詰められる。
- `|` を含む Markdown 敵対的な関数名 → エスケープされる。
- 全 100% fixture → 祝いメッセージ・表なし。

### 4. ドキュメント更新

`document/06-conventions.md` の CI 表 / coverage 説明を更新: 失敗レポートが**未達ファイル＋関数**を列挙し、Job Summary で fork-safe、上限付き、sticky コメントで重複防止であること、新スクリプトとその test を記載。Markdown link check 対象。

## 受け入れ条件

- カバレッジ <100% の PR で、対象 PR コメント（同一リポジトリ）**および** Check/Job Summary（fork 含む）に、未達ファイルの path・未達関数名・関数率/行率・不足量が上限付きで表示される。
- 同一 PR の再実行でコメントが重複せず 1 件に更新される（sticky `recreate`）。
- **coverage の gate（exit code）はコメント投稿の成否から独立**。fork PR ではコメントが投稿できなくても job は正しく pass/fail し、Job Summary に一覧が出る。
- **失敗時（<100%）にも必ずレポートが出力**され、その後 enforce が job を落とす。lcov が生成できない compile/test 失敗時は従来どおり test step で落ちる。
- 出力は `MAX_FILES` / `MAX_FUNCS_PER_FILE` で上限を持ち、超過分は「…ほか N 件」で明示され黙って切り詰めない。
- 既存の **coverage 100% gate（`scripts/coverage.sh` の SSoT・lines/functions=100）を壊さない**。
- 生成スクリプトが fixture ベースの test で検証され、`test.yml` の script-tests job で実行される。

## テスト方針

- `bash scripts/tests/coverage-report-comment.sh`（fixture lcov → 期待 Markdown）。
- YAML 妥当性・ワークフロー step 条件の確認。
- docs 差分は Markdown link check（lychee）。
- Rust 差分なし（CI glue と Ruby スクリプトのみ）なら Rust full gate/coverage は非該当。完了報告に該当有無を明記する。

## 非目標

- 100% 閾値・enforce の意味論の変更（lines+functions=100 の SSoT は維持）。
- diff/branch スコープのカバレッジ、履歴トレンド、外部カバレッジサービス連携。
- 生成ツールの Rust 実装。
- coverage 以外の `test.yml` 挙動変更。
