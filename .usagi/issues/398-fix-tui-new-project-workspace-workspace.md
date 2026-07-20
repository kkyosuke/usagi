---
number: 398
title: fix(tui): New Project の workspace 作成を副作用前に事前検証し既存 workspace・不正パスを弾く
status: done
priority: high
labels: [tui, core, bug, validation, ux]
dependson: []
related: [357, 370, 369, 375]
created_at: 2026-07-20T04:39:41.206922+00:00
updated_at: 2026-07-20T05:01:44.785562+00:00
---

## 背景 / 問題

TUI の New Project 画面（Welcome → New。`crates/tui/src/presentation/views/new.rs`）は
workspace を新規作成する入口で、Clone（repository を子ディレクトリへ clone）と
Existing（既存ディレクトリを登録）の 2 モードを持つ。Enter による作成確定は #357 で配線済みだが、
**事前検証が「必須フィールドが trim 後に非空か」だけ**で、次の gap がある。

- **「workspace がすでに存在する」を検出しない**。
  - Existing: `workspace_usecase::register`（`crates/core/src/usecase/workspace.rs`）は
    **同一 path の既存 entry を黙って再利用**し、名前衝突は `-2` `-3` の suffix で回避する。
    つまり「すでに登録済みの workspace」を選んでもエラーにならず、利用者は何が起きたか分からない。
  - Clone: `validate_new_form`（`crates/tui/src/usecase/application/controller.rs`）は destination の
    存在を見ない。合成ルート `create_workspace`（`src/runtime/tui.rs`）が
    **`std::fs::create_dir_all(parent)` を実行してから `git clone`** するため、destination が既存の
    ときは **ディレクトリ作成という副作用を起こした後**に git が失敗する。「作成処理へ進む前に弾く」に
    なっていない。
- **パスの不正を事前検証しない**。Clone の `directory` に `/`・`\` や `.`・`..` が入っても
  `PathBuf::from(location).join(directory)` がそのまま親を跨ぐ path を作る。Existing の path が
  ディレクトリでない/存在しない場合の判定は合成ルート `validate_workspace_directory` の実行時のみで、
  事前検証（入力画面に留まって修正）に統一されていない。
- **境界条件が未整理**。前後空白は `trim` されるが、同名/同 path 判定・Unicode を含む名前の扱いが
  テストで固定されていない。

session 名の inline validation は #370 で完了済み（空 = Enter 時 / 不正文字 / 64 超過 / 表示中 session と
同名）。本 issue は **workspace 側**を同じ水準へ引き上げ、「入力された workspace がすでに存在するときは
作成処理へ進まずエラーを表示する」を満たす。

## ゴール

New Project の作成を、**副作用（`create_dir_all` / `git clone` / registry 書き込み）の前に事前検証**し、
既存 workspace・不正パスを安全・具体的なメッセージで弾く。失敗時は入力画面に留まり draft を保って
再試行でき、検証失敗時に一切の副作用を起こさない。

## スコープと方針（SSoT を保つ pure 検証 + 合成ルートの事前 guard）

作成の live path は実端末経路の `loader.create_workspace`（`src/runtime/tui.rs::FsWorkspaceLoader`）で、
controller-runtime 側の `Effect::CloneProject` / `Effect::RegisterWorkspace` は現状 no-op
（`presentation/mod.rs:1842` / `daemon_backend.rs:374`）。したがって本 issue は live path を正とする。

### 1. core に pure な事前検証を追加（`crates/core/src/usecase/workspace.rs`）

実 IO（存在確認）を持たない **pure な判定関数**を core に置き、100% test する。合成ルートが
FS 事実（`std::fs` の probe）と registry（`load_workspaces`）を集めてこの関数へ渡す。

- `pub enum WorkspaceProbe { Missing, Directory, NonDirectory }`
- `pub enum NewWorkspaceError { CloneDestinationExists, ExistingPathMissing, ExistingPathNotDirectory, AlreadyRegistered }`
  - 各 variant に **1 行・安全・具体的**な `message()`（notice slot は 72 桁で 1 行化されるため簡潔に）:
    - `CloneDestinationExists` → `"a directory already exists at the clone destination; choose another location or directory name"`
    - `ExistingPathMissing` → `"directory not found; enter a path that exists"`
    - `ExistingPathNotDirectory` → `"path is not a directory; choose a directory"`
    - `AlreadyRegistered` → `"this directory is already a registered workspace"`
- `pub fn preflight_new_workspace(kind: NewWorkspaceKind, target: &Path, probe: WorkspaceProbe, registered: bool) -> Result<(), NewWorkspaceError>`
  - Clone（target = destination）: `registered` → `AlreadyRegistered`; probe != `Missing` → `CloneDestinationExists`; それ以外 `Ok`。
  - Existing（target = path）: probe `Missing` → `ExistingPathMissing`; `NonDirectory` → `ExistingPathNotDirectory`; `registered` → `AlreadyRegistered`; それ以外 `Ok`。
- 登録済み判定は usecase の identity（**exact `Path` equality**、`resolve_or_register` と同じ）に揃える。

### 2. TUI の pure 検証を強化（`controller.rs::validate_new_form` / `NewValidationError`）

外部状態を要さない **フォーム内で完結する**不正パス検出を追加（入力を失わず、即時・SSoT）。

- Clone の `directory` が path separator（`/` `\`）を含む、または `.` / `..` のとき
  → 新 variant `DirectoryInvalid`（message: `"directory name must not contain path separators"`）。
- 既存の required（trim 後非空）と field 別メッセージは維持。`New::to_request` 経由で実端末経路と
  controller-runtime が同じ規則を共有する（SSoT）。

### 3. 合成ルートで副作用の前に guard（`src/runtime/tui.rs::create_workspace`、coverage-off IO）

- Clone: destination を `std::fs` で probe → `WorkspaceProbe`、`load_workspaces` に destination が
  あるか → `registered`、`preflight_new_workspace(Clone, …)?` を **`create_dir_all` / `git clone` より前**に呼ぶ。
- Existing: path を probe（`validate_workspace_directory` を置換/包含）、registry を照合、
  `preflight_new_workspace(Existing, …)?` を **`register` より前**に呼ぶ。
- `Err` は安全な 1 行 `io::Error` にして返す。実端末経路の `NewStep::Create` 失敗枝
  （`presentation/mod.rs:2547`）が既に **draft を保持したまま notice を出して同画面に留まる**ため、
  副作用ゼロで「留まる + 再試行」を満たす（`new_project_notice` が 1 行 72 桁へ安全化）。

## 境界条件

- 前後空白: 入力は `trim` してから判定。trailing/leading space だけの差の path/directory が既存と同一視される。
- Unicode: workspace 名（display 名）に charset 制約は課さない（不当に弾かない）。Unicode を含む directory 名も
  separator/dot を含まなければ受理。重複判定は正規化せず exact 比較で決定的にする。
- 同名/同 path: exact `Path` equality。既存 registry の同 path は `AlreadyRegistered`。

## テスト（決定的）

- **core（pure, 100%）**: `preflight_new_workspace` を Clone/Existing × {成功 / 既存(登録済み) /
  destination 既存 / path 欠落 / path 非ディレクトリ} で固定。`NewWorkspaceError::message` の各分岐。
  trailing-space 付き path が既存と一致すること、Unicode path が誤判定されないこと。
- **TUI（pure）**: `validate_new_form` の `DirectoryInvalid`（`/`・`\`・`.`・`..`）と既存 required 表を維持。
- **TUI runtime（実端末経路）**: `FakeLoader` に create 失敗注入を足し、既存 workspace 相当の `Err` で
  (a) notice が出る (b) draft が保たれる (c) 入力を直して再 submit すると成功して Home へ遷移する、を固定。
  検証失敗時に create が呼ばれない（副作用なし）ことを確認。

## ドキュメント

- `document/03-tui.md` の New 画面（Welcome → New）節に、作成前の事前検証（既存 workspace / 不正パス /
  欠落フィールドは副作用前に弾く・失敗時は draft 保持で同画面に留まり再試行）を実装済み挙動として明記する。

## 受け入れ条件

- 既存 workspace（登録済み path・clone destination が既存）を入力して Enter → 作成へ進まず、具体的な
  エラーが表示され、`create_dir_all` / `git clone` / `register` の副作用が発生しない。
- 不正パス（separator/dot を含む directory・存在しない/非ディレクトリの path）→ 具体的なエラーで同画面に留まる。
- 空入力 → 従来どおり field 別エラー。
- エラー後に入力を修正して再 submit すると作成が成功し Home へ遷移する。
- 上記テストが通り、coverage 100% と規約 gate（fmt / clippy / full test / Markdown link check）を満たす。
