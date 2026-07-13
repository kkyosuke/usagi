# 4. オーケストレーション（セッション・worktree 管理）

> [ドキュメント目次](README.md) ｜ ← 前へ [3. コマンドリファレンス](03-commands/README.md) ｜ 次へ → [5. 設定](05-settings.md)

`usagi` の中核は、**複数の作業を worktree ベースの「セッション」として束ね、複数リポジトリ構成でも
一括でオーケストレーションする**ことです。本書はその概念モデルとライフサイクルをまとめます。各コマンドの
構文は [3. コマンドリファレンス](03-commands/README.md)、画面の操作は [design/home/README.md](design/home/README.md)、
永続化されるデータは [data/02-workspace.md](data/02-workspace.md) を参照してください。

## 目次

- [用語](#用語)
- [自律オーケストレーション運用モデル](#自律オーケストレーション運用モデル)
- [なぜ worktree を 1 か所に集約するのか](#なぜ-worktree-を-1-か所に集約するのか)
- [セッションの構築（再帰走査と複数リポジトリ対応）](#セッションの構築再帰走査と複数リポジトリ対応)
- [複数ワークスペースの統合（unite）](#複数ワークスペースの統合unite)
- [新ブランチの基点（local / remote）](#新ブランチの基点local--remote)
- [state.json との同期（孤児セッションの掃除）](#statejson-との同期孤児セッションの掃除)
- [スキルの配布](#スキルの配布)
- [セッションのライフサイクル](#セッションのライフサイクル)
- [アクティブなセッションと AI 連携](#アクティブなセッションと-ai-連携)
- [ペインの復旧](#ペインの復旧)
- [キュー済みプロンプトの自動起動](#キュー済みプロンプトの自動起動)

## 用語

| 用語 | 意味 |
|---|---|
| ワークスペース | usagi に登録したプロジェクトのルートディレクトリ。git リポジトリでなくてもよい（複数リポジトリのルートでも可）。グローバルレジストリ `workspaces.json` に登録される |
| セッション | 1 つの作業単位。`session create <name>` でワークスペースルート配下に作られる worktree 群（＋コピー）の集合。名前 `<name>` で識別する |
| worktree | git の作業ツリー。各 git リポジトリにつき 1 つ、セッション用ブランチをチェックアウトして作られる |
| アクティブなセッション | `session overview` で選択中の作業対象。`terminal` / `agent` の実行カレントディレクトリになる |
| ルート行 | どのセッションにも属さない常設の行（`⌂ root`）。選ぶと `terminal` / `agent` がワークスペースルートで起動する |

## 自律オーケストレーション運用モデル

usagi の自律オーケストレーションは、次の 3 原則の上に成り立ちます。これらは規約ではなく[ガードレール](#ガードレール多層防御)で
**技術的に担保**されます。

1. **root（ルート行で動くコーディネータ）はオーケストレーションのみを行う**。issue の選択・順序付け、
   session の作成/委譲（`session_delegate_brief` / `session_delegate_issue` / `session_create` / `session_prompt`）、
   進捗ポーリング（`session_status` / `session_pr`）、完了 session の除去（`session_remove`）、次タスクの投入。
2. **root は git 追跡下のリポジトリを一切変更しない**。issue の作成・本文/`status` 編集、ドキュメント編集、
   コード編集、`main` への commit / PR は root では行わない。
3. **リポジトリに変更が入りうる作業（調査→issue 化・実装・修正・ドキュメント更新）は必ず session の
   worktree（ブランチ）で行い、PR で `main` に反映する**。

この分界は、MCP の routing 非対称性（issue / memory はカレント worktree、session は workspace root に解決。
[03-mcp.md#起動と登録](03-commands/03-mcp.md#起動と登録)）を土台にしています。root で起動したときは
`worktree == workspace_root` が一致し、これが「root で動いている」の機械判定になります。

### root と session の責務分界

| 操作 | root（workspace root） | session（worktree） |
|---|---|---|
| issue の選択・順序付け | ✅（`issue_search` / `issue_get`） | —（自分のタスクに集中） |
| issue の起票・本文編集 | ❌ | ✅（トリアージ/設計 session が起票） |
| issue の `status` 更新 | ❌ | ✅（**自分の issue のみ**・自枝で） |
| session 作成・委譲・プロンプト投入 | ✅ | ✅（サブ委譲も可） |
| 進捗ポーリング（phase・git 状態・PR） | ✅ | ✅ |
| 完了 session の除去 | ✅ | —（自分は消さない） |
| コード・ドキュメント編集 | ❌ | ✅ |
| `main` への commit / PR | ❌ | ✅（PR 経由） |

### 起源フローと遂行フロー

作業は **起源（A）** と **遂行（B）** の 2 経路で生まれます。どちらも root は「起こして・見て・片付ける」だけで、
リポジトリを変えるのは常に session です。

```text
root（⌂ root 行）
  ├─ (A) session_delegate_brief(brief) ─▶ triage session
  │        調査 → issue_create（自枝）→ PR                  ── merge ──▶ main の backlog に issue が現れる
  └─ (B) session_delegate_issue(number) ─▶ work session（issue-N）
           着手で status=in-progress → 実装＋PR 前に status=done（自枝）→ PR ── merge ──▶ main に done が乗る
```

- **(A) 起源（`session_delegate_brief`）**: root は事前 issue を必要としない**自由記述のブリーフ**を新規 session に
  渡します。tool は `session_create` → `session_prompt(mode=queue)` を合成し、ブリーフをトリアージ session 用の
  定型指示でラップして起動時キューに積むだけです（[03-mcp.md#session_delegate_brief-の挙動](03-commands/03-mcp.md#session_delegate_brief-の挙動)）。
  その session が worktree 内で調査し `issue_create` で起票して PR し、マージで `main` の backlog に現れます。
  root が issue を作れない（原則 2）なかで作業を生む唯一の入口です。
- **(B) 遂行（`session_delegate_issue`）**: `main` にコミット済みの issue を新 session に委譲します。tool は
  委譲先ワークツリーの基点コミットに issue ファイルが含まれるかを検証し、未コミットなら拒否します
  （[03-mcp.md#session_delegate_issue-の挙動](03-commands/03-mcp.md#session_delegate_issue-の挙動)）。これにより
  「未コミット issue が新 worktree の枝に乗らず、session が `status` を更新できない」不具合（#104）を根治します。

### status ライフサイクル（単一書き手）

`status` の**書き手は常に当該 issue の session だけ**です（[.agents/workflow.md](../../.agents/workflow.md)）。root は
原則 2 で `status` を書けないため、**session が生きているうちに自枝で `done` を立て、PR（マージ）で `main` に運ぶ**のが
唯一整合する経路です。

| 遷移 | 誰が | どこで | いつ |
|---|---|---|---|
| `todo`（起票時） | 起源 session | 自枝 | 起票 PR に含める |
| `todo` → `in-progress` | 委譲された session | 自枝 | 着手時 |
| `in-progress` → `done` | 委譲された session | 自枝 | **PR を開く前**（実装差分と同じ PR に status 差分を載せる） |

- `in-progress` は `main` に遅れて届く（マージ後＝実際は完了後）ため、root はこれを当てにせず、
  **「その issue の session が生存しているか」を in-progress の実効シグナル**にします（`session_list` /
  `session_status`、命名規約 `issue-<番号>`）。root は「`main` で `todo` かつ生存 session が無い issue」だけを
  ready 候補として委譲し、二重委譲を避けます。
- `done` は PR に載って `main` に届きます（委譲プロンプトの status 指示は
  [03-mcp.md#session_delegate_issue-の挙動](03-commands/03-mcp.md#session_delegate_issue-の挙動)）。root は
  `session_status.merged` / `session_pr` で取り込みを検知して `session_remove` → 次へ進みます。

### ガードレール（多層防御）

「root は repo を変更しない」を、規約だけでなく多層で技術的に担保します。

| 層 | 仕組み | 塞ぐ経路 | 判定軸 |
|---|---|---|---|
| A（一次） | MCP 書き込み拒否 — 合成層が `worktree == workspace_root` のとき書き込み系 issue tool を拒否（[03-mcp.md#ルートでの書き込みガードレール](03-commands/03-mcp.md#ルートでの書き込みガードレール)） | MCP 経由の issue 変更 | `worktree == workspace_root` |
| B（一次） | guard-workspace の root モード — root 行の Agent の `PreToolUse` で Edit/Write と変更系 git を拒否（[worktree への閉じ込め](#worktree-への閉じ込めメインリポジトリ保護)） | 直接のファイル編集・`git commit` 等 | cwd が `.usagi/sessions/` 配下でない |
| C（backstop） | pre-commit — 非セッションチェックアウトのコミットを拒否（[06-conventions.md#git-hookslefthook](06-conventions.md#git-hookslefthook)） | 人手・別ツールのコミット | worktree が `.usagi/sessions/` 配下か |
| D（リモート） | `main` ブランチ保護 — 直 push 禁止＋PR 必須（[06-conventions.md#cigithub-actions](06-conventions.md#cigithub-actions)） | リモートの直更新 | サーバ側 |

A と B は判定軸が別（MCP プロセス側の path 一致 / Agent の cwd）で互いに独立に効くため、一方をすり抜けても
他方が捕まえます。読み取り・整形・session 操作（`issue_search` / `issue_get` / `issue_to_prompt` /
`memory_*` / すべての `session_*`）は root でも許可し、オーケストレーションを妨げません。

## なぜ worktree を 1 か所に集約するのか

usagi は worktree を **リポジトリ任意の場所ではなく、ワークスペースルート直下の
`.usagi/sessions/<name>/` に集約** して管理します。これにより、

- セッションの所在が一意に定まる（どこに作られたか探さなくてよい）。
- 一覧・削除・クリーンアップが扱いやすくなる。
- `.usagi/` は `.gitignore` 済みのため、各 worktree がワークスペースのコミット対象に混入しない。

## セッションの構築（再帰走査と複数リポジトリ対応）

ワークスペースのルート自体が git リポジトリである必要はありません。`session create <name>` は
ルートを**再帰的に走査**し、各エントリを次のように扱います。

- **git リポジトリのディレクトリ** → そのリポジトリの `git worktree` を
  `.usagi/sessions/<name>/<相対パス>/` に、新しい `usagi/<name>` ブランチを切って作成する。
  ブランチ名はセッション名 `<name>` を `usagi/` 名前空間に収めたもので、手で切った
  ブランチ（素の `<name>` や `feat/…` など）と衝突しないようにしている。worktree の
  ディレクトリ・セッション名・サイドバー表示は `usagi/` を付けない `<name>` のまま。
  worktree 作成直後に submodule を初期化・チェックアウトする（`git submodule update --init --recursive` 相当）。
  submodule を持つリポジトリではそのまま作業でき、持たないリポジトリ（`.gitmodules` がない）では何もしない。
  破棄時もこの worktree を問題なく取り除く（git は submodule を含む worktree の削除を素の `git worktree remove` では一律拒否するが、クリーンであることを確認した上で内部的に強制削除する。未コミット変更があれば従来どおり `--force` が必要）。
- **既存のリンク worktree**（`.git` がディレクトリでなくファイル＝他所で管理されている
  `git worktree`。例: `.workspace`、`.claude/worktrees/*`）→ 走査対象から除外し、複製も
  ブランチ作成もしない。
- **git 管理外のファイル・ディレクトリ** → 同じ相対パス `.usagi/sessions/<name>/<相対パス>/` へコピーする。

> `.git` / `.usagi` も走査対象から除外されます。

これにより、単一リポジトリだけでなく、ルートが git でない複数リポジトリ構成（モノレポ的な
ディレクトリツリー）にも対応できます。

```text
/root                         （git でなくてもよい）
├── app-a/      = git    → app-a の worktree を作成
├── app-b/      = git    → app-b の worktree を作成
├── be/                  （git でない素のディレクトリ → 再帰）
│   └── be1/    = git    → be/be1 の worktree を作成
└── README.md            （git 管理外 → コピー）
```

セッション `feature-x` を作成すると、`.usagi/sessions/feature-x/` 配下にルートと同じディレクトリ
構造が再現され、git 配下の各サブディレクトリはそれぞれ `usagi/feature-x` ブランチの worktree、それ以外は
コピーになります。各 worktree の状態は `state.json` の該当セッション（`SessionRecord`）の
`worktrees` 配列（`WorktreeState`）に記録されます（`path` が `.usagi/sessions/<name>/...` を指す）。
データ構造は [data/02-workspace.md](data/02-workspace.md) を参照してください。

セッション名は `session create <name>` の引数で渡すほか、名前を省くと[選択（Overview）モード](design/home/02-layout.md#選択overview既定)の
左ペイン内インライン入力で指定できます。次の名前はバリデーションで弾かれます。

- **空文字・パス区切り**（`/` `\` `.` `..`）を含む名前。
- **`-` で始まる**名前。セッション名は worktree のパスや `usagi/<name>` ブランチ名の一部として git コマンドの引数に渡るため、先頭が `-` だと git にオプション（`-D` など）と誤認される。
- **既存セッションと重複**する名前。
- **既存ブランチの名前空間と衝突**する名前。セッションは各リポジトリで `usagi/<name>` ブランチを切るため、
  すでに `usagi/<name>/…` 配下にブランチがあると git が `usagi/<name>` ブランチを作れない。
  この場合は作成前に衝突しているブランチ名を示して中断する（別のセッション名を選ぶ）。
  なお `usagi/` 名前空間に収めることで、`<name>/…`（例: `test/foo`）のような**手で切った
  ブランチとは衝突しなくなる**。

## 複数ワークスペースの統合（unite）

usagi は**複数のワークスペースを 1 つのホーム画面にまとめて**操作できます（統合 / unite モード）。
[プロジェクト選択画面（Open）](design/02-open.md#統合uniteモードで開く)で `Space` により複数の
ワークスペースをチェックして `Enter` で同時に開くか、開いた後に[コマンドパレット](03-commands/02-tui.md#unite)の
`unite add` / `unite remove` で足し引きします。

- **1 つのワークスペース**を開けば従来どおりの単一ホーム。**2 つ以上**を開くと、左ペインは
  ワークスペースごとの**グループ**を積み重ねて表示します（各グループ＝ワークスペース名のヘッダ＋
  その `⌂ root` 行とセッション群）。見た目とキー操作は
  [design/home/03-sidebar.md#統合uniteモードの積み重ね表示](design/home/03-sidebar.md#統合uniteモードの積み重ね表示)が正本です。
- **コマンドの対象解決**: 新規セッション作成はカーソルがいるグループのワークスペースに作られ、削除・表示名・
  メモ・root 行の `terminal` / `agent` は対象セッション（行）が属するワークスペースに作用します。各ワークスペースは
  自分の `.usagi/`（`state.json` / `settings.json` / issue / memory）をそれぞれ持つため、セッションのライフサイクルは
  ワークスペース単位で独立しています。
- **直近の組み合わせの記憶**: 最後にまとめて開いたワークスペースの集合を保存し、次回 Open 画面で
  あらかじめチェックします（保存先は [data/01-global.md#unite-setjson直近の統合セット](data/01-global.md#unite-setjson直近の統合セット)）。
- 各ワークスペースの worktree は引き続きそれぞれの `.usagi/sessions/<name>/` に集約され、統合はあくまで
  **表示と操作を 1 画面に束ねる**ものです。ターミナル・Agent・状態監視は worktree の絶対パスをキーに動くため、
  複数ワークスペースが混在しても取り違えません。

## 新ブランチの基点（local / remote）

新しい `usagi/<name>` ブランチを**どの基点から切るか**は、各リポジトリのローカル設定 `default_branch`（基点ブランチ）
と `default_branch_source`（その基点を `local` 形・`remote` 形のどちらで解決するか）で決まります。**各設定の
意味・既定値・選択肢・フォールバック順は
[05-settings.md#ローカル設定（プロジェクト単位の上書き）](05-settings.md#ローカル設定プロジェクト単位の上書き) が正本**です。

設定は**リポジトリ単位**です。複数リポジトリ構成では `session create` 実行時に各リポジトリの
`<repo>/.usagi/settings.json` をそれぞれ参照し、リポジトリごとに異なる基点で worktree を切れます。基点解決は
`infrastructure/git.rs` の `resolve_base_ref`、適用は `usecase/session` の `create` / `build_dir` が担います。

## セッション作成後のセットアップコマンド

ワークスペースのローカル設定 `setup_commands` にコマンド列がある場合、`session create` は worktree / コピー済み
ファイル / 同梱スキルのリンクを作成したあと、セッション root（`<workspace>/.usagi/sessions/<name>`）を
カレントディレクトリとしてコマンドを保存順に実行します。設定の意味・保存形式・編集方法は
[05-settings.md#ローカル設定（プロジェクト単位の上書き）](05-settings.md#ローカル設定プロジェクト単位の上書き) が正本です。

- コマンドは 1 要素 = 1 shell コマンド行で、Unix 系では `sh -lc`、Windows では `cmd /C` で実行します。
- 空白だけのコマンドは実行しません。
- コマンドが失敗しても作成済みセッションは削除せず、エラーログとトレースログに記録して次のコマンドへ進みます。
  セッション自体はそのまま残るため、ユーザーは対象セッションを開いて原因を確認・修正できます。

## state.json との同期（孤児セッションの掃除）

`session create` / `session remove` の実行時に、`.usagi/sessions/` 配下のディレクトリと `state.json` の記録を照合します。**`state.json` に記録のないディレクトリ**（中断された作成・手で編集された `state.json`・クラッシュなどで取り残されたもの）は「孤児」とみなし、**未コミット変更の有無にかかわらず強制削除**して同期を取ります（worktree の登録解除・セッションブランチの削除・コピーしたファイルの除去）。

- これにより、作成時は同名の取り残しディレクトリが新規セッションの作成を妨げません。
- セッションの掃除では、そのセッションパス配下にある worktree を**ブランチ名が一致するかどうかに依らず**登録解除します。これがないと、想定外のブランチに切り替わった worktree（例: セッション内で別ブランチを切ったもの）はディレクトリだけ削除され、**git の worktree 登録だけが取り残されて**しまいます。
- 加えて `session create` は、各リポジトリで**実体ディレクトリの消えた worktree 登録（dangling 登録）を作成前に `git worktree prune` で掃除**します。これがないと、同じセッションパスに対する `git worktree add` が「missing but already registered worktree」で失敗し、同名セッションを二度と作れなくなります。
- 記録済みセッション本体の削除には引き続き未コミット変更のガード（`--force` 必須）が効きます。掃除されるのは **記録のない** ディレクトリだけです。
- 逆に、**`state.json` に記録はあるが worktree 実体が無い**セッション（作成が途中で中断され worktree が構築されなかった、あるいは手で消されたもの）の削除も滞りません。git の worktree 登録解除が対象不在で失敗しても、その後始末はスキップして記録自体は確実に取り除きます。
- セッションディレクトリ直下の単なるファイルは対象外です。

## スキルの配布

usagi はバイナリに**スキル**（Claude Code の `SKILL.md`）を同梱し、起動した Agent へ配布します。スキルは
`assets/skills/<name>/SKILL.md` としてビルド時に埋め込まれ、`infrastructure/skills.rs` が次の 2 段で届けます。

同梱スキルは次の通りで、`usagi-session` 以外は**機能（feature）単位**で ON/OFF できます（[設定](05-settings.md#設定項目)）。

| スキル | 機能 | 役割 |
|---|---|---|
| `usagi-session` | （なし・常時 ON） | セッション worktree での作業規約 |
| `usagi-pr-create` | `pull-request` | PR を新規作成する手順 |
| `usagi-pr-update` | `pull-request` | PR の概要更新・レビュー返信 |
| `usagi-pr-fix` | `pull-request` | レビュー対応・最新化・コンフリクト解消 |

```
バイナリ埋め込み (assets/skills/)
      │ TUI / MCP 起動時に materialize
      ▼
~/.usagi/skills/<name>/SKILL.md          ← スキルの唯一の実体（正本）
      ▲ symlink（session create 時、スキルごと）
<worktree>/.claude/skills/<name> ─────────┘
```

1. **展開（materialize）**: TUI（`hop`）・MCP サーバ（`mcp`）の起動時に、埋め込んだスキルを
   `~/.usagi/skills/`（[data/01-global.md#skillsagent-へ配布するスキル](data/01-global.md#skillsagent-へ配布するスキル) が正本）へ
   冪等に展開する。バイナリ更新後の再起動で内容が更新される。
2. **symlink（session create）**: セッション作成時、各 worktree の `.claude/skills/<name>` を上記の各スキルへの
   symlink として張る（ディレクトリ全体ではなく**スキルごと**）。worktree ごとにコピーせず、正本 1 か所を
   全 worktree が参照する。Agent は cwd（worktree）直下の `.claude/skills` から自動的にスキルを発見する。
   このとき、ワークスペースの**実効設定**（グローバル ⊕ ローカル上書き）で**機能が無効なスキルは symlink しない**。
   `materialize` は機能の ON/OFF に関わらず全スキルを展開するので、後から機能を ON にした新規セッションでは
   そのまま配布される。`usagi-session` は機能に属さず常に配布される。

- **プロジェクト独自のスキルと共存**: symlink はスキル単位で張るため、ユーザーが用意した
  `.claude/skills/<別名>` と usagi のスキルは同じディレクトリに並んで共存する。usagi が張るのは埋め込み
  スキルの名前のエントリだけで、**同名の実体（ファイル/ディレクトリ）が既にある場合は上書きせずそのまま残す**
  （古い usagi の symlink だけは現在の正本へ張り替える）。
- **git から隠す**: 各 symlink は git 管理外（untracked）なので、そのままだとセッションが
  「未コミット変更あり」と判定され `remove` / `finish` を妨げ、TUI 上もダーティ表示になる。これを避けるため、
  symlink を張ると同時に worktree のローカル除外（`$GIT_DIR/info/exclude`）へ `/.claude/skills/<name>` を
  スキルごとに追記する。除外はリポジトリローカルでコミット・push されず、ユーザーの追跡対象 `.gitignore`
  にもユーザー独自のスキルにも触れない（`infrastructure/git.rs` の `ensure_excluded`）。
- いずれの段もベストエフォートで、失敗してもセッション作成・起動は止めない。

## セッションのライフサイクル

セッションは「作成 → 作業 → 破棄」で完結します。各操作は[ホーム画面](design/home/README.md)の `session` /
`terminal` / `agent` コマンドで行います。

```text
  session create <name>        terminal / agent          session remove <name>
        │                          │                            │
        ▼                          ▼                            ▼
   [セッション作成] ───────▶ [作業（worktree 上で          ───▶ [worktree・ブランチ・
   （再帰走査・worktree 構築）   シェル / Agent を起動）]          コピーを削除］
```

| 段階 | コマンド | 役割 |
|---|---|---|
| 作成 | `session create [<name>]` | ルートを再帰走査して `.usagi/sessions/<name>/` 配下に worktree 群を構築 |
| 一覧 | `session list` | セッション一覧（件数・各セッション名・worktree 数）を表示 |
| 選択 | `session overview [<name>]` | アクティブなセッションを切り替え（引数なしで[選択](design/home/02-layout.md#選択overview既定)モード） |
| 作業 | `terminal` / `agent` | アクティブな worktree でシェル / Agent CLI を右ペインに埋め込み起動 |
| 状態確認 | `usagi status` | 各 worktree のブランチ・`local` / `pushed` / `merged` 状態を同期・表示 |
| 破棄 | `session remove [<name>] [--force]` | worktree・ブランチ・コピー・会話履歴と、その worktree をキーにした usagi の一時ファイル（Agent phase・PR リンク・キュー済みプロンプト・ペイン構成）を削除（未コミット変更があれば `--force` 必須） |

`session` のサブコマンドは短縮形を受け付けます（`create`=`c`/`new`、`list`=`ls`、`remove`=`rm`）。

セッションは**作成時に「誰が起動したか」を記録**します。人がホーム画面の[選択（Overview）](design/home/02-layout.md#選択overview既定)で
作成すれば `human`、エージェントが MCP の `session_create` / `session_delegate_issue` / `session_delegate_brief` で作成すれば `mcp` が
`state.json` の [`SessionRecord.origin`](data/02-workspace.md#セッションごとsessionrecord) に一度だけ書き込まれ、
以後（セッション切り替え・メモ編集・同期など）変わりません。コーディネータ役のエージェントは `session_list` / `session_status` の
返す `origin` で、自分が委譲した自動セッションと人が手で作ったセッションを区別できます。この項目を持たない古い
`state.json` のセッションは `unknown`（未記録）に縮退します。

あわせて、セッションが**どのセッションから開始されたか**（親セッション名）も記録します。エージェントが
**あるセッションの中から** MCP の `session_create` / `session_delegate_issue` / `session_delegate_brief` を呼ぶと、その親セッション名が
`state.json` の [`SessionRecord.started_from`](data/02-workspace.md#セッションごとsessionrecord) に一度だけ書き込まれます
（MCP サーバは自分が動いているセッションを worktree のパスから判定します）。人が TUI で作成した場合や、エージェントが
ワークスペースルート（どのセッションにも属さないコーディネータ）から作成した場合は親が無く `null` です。これにより
コーディネータは `session_list` / `session_status` の `started_from` を辿って、委譲した子セッションの系譜（どの
セッションがどのセッションを生んだか）を再構成できます。
子セッションが MCP `session_complete(message)` を呼ぶと、この `started_from` を return address として親 session へ、
値が無ければルート行へ完了報告を送ります。agent が宛先を判断・指定する必要はありません。

## アクティブなセッションと AI 連携

- `session overview` で選択したセッションが「アクティブ」になり、ホーム画面の左ペインで `*`（緑）と太字で強調されます。
- `terminal` / `agent` はアクティブな worktree（ルート行選択時はワークスペースルート）をカレントディレクトリに実行します。
- `agent` は設定の Agent CLI（`claude` / `codex` / `gemini` など。[5. 設定](05-settings.md)）を埋め込みシェルで起動し、
  usagi の MCP サーバ（`usagi mcp`。[3.3 MCP サーバ](03-commands/03-mcp.md)）を組み込みます。
  これにより Agent 自身が、issue / memory の操作に加えて `session_create` / `session_list` / `session_prompt` /
  `session_pr` / `session_remove` で並行セッションを作成・委譲（`session_prompt` の `mode` で起動時キュー / live agent への送信を選択）・PR 参照・整理できます（別セッションのエージェントへのタスク委譲・不要セッションの削除）。issue を新しいセッションへ丸ごと委譲する定番手順は `session_delegate_issue`、事前 issue なしでトリアージ session を起こす起源フローは `session_delegate_brief` の 1 呼び出しにまとまっています。委譲先の進捗はコーディネータが `session_status` でポーリングできるほか、子セッションのエージェントが `session_complete` で**記録済みの呼び出し元へ完了を push で報告**できます（ポーリング間隔を待たずに完了が届く。[3.3 MCP サーバ#session_complete の挙動](03-commands/03-mcp.md#session_complete-の挙動)）。ローカル LLM が有効なら
  `usagi llm-mcp` も組み込み、軽量タスクをローカル LLM へ委譲してクラウド Agent のトークン消費を抑えます
  （[3.4 ローカル LLM MCP サーバ](03-commands/04-llm-mcp.md)）。
- `agent` は対象 worktree に前回の会話が残っていれば**前回セッションの続きから**起動し、無ければ通常起動します
  （CLI ごとの再開フラグ・キュー済みプロンプト（`session_prompt`）との両立可否など挙動の正本は
  [3.2 TUI 内コマンド#agent](03-commands/02-tui.md#agent)）。
- **セッション単位のエージェント CLI・モデル指定**: `session_create` / `session_delegate_issue` / `session_delegate_brief`（MCP）で作成・委譲する際に
  任意の `agent_cli` / `model` を渡すと、その値が `state.json` の `SessionRecord.agent`（[data/02-workspace.md](data/02-workspace.md#セッションごとsessionrecord)）に記録され、そのセッションのエージェント起動時に**ワークスペースの実効設定 `agent_cli` より優先**して解決されます
  （集中からの起動・[ペインの復旧](#ペインの復旧)・[キュー済みプロンプトの自動起動](#キュー済みプロンプトの自動起動)のいずれでも同じ）。CLI 解決の優先順位は
  **集中での明示選択（`agent <name>`）＞セッションの `agent`＞ワークスペースの実効 `agent_cli`**、モデルはセッションの `agent.model`
  を各 CLI のモデルフラグ（claude `--model`、codex / gemini `-m`）へ展開します。コーディネータが「軽いタスクは小さいモデル、
  重い設計は大きいモデル」とタスクごとに振り分けるための仕組みです。未指定なら従来どおり実効設定と各 CLI の既定モデルに従います。

各 worktree のシェル / Agent は TUI が所有する PTY で動き、ターミナルプールが worktree パスをキーに保持します。セッションを切り替えても同じ TUI の間は端末と Agent CLI が走り続けますが、TUI を閉じると PTY とその子プロセスも終了します。`session remove` は worktree 配下のシェル / Agent、会話履歴、Agent phase・PR リンク・キュー済みプロンプト・ペイン構成を削除します。

merged になった PR や終了済み Agent のペインは、セッション本体を残したまま閉じてプロセスだけ回収できます。
選択（Overview）の `X` は、PR badge が merged のセッション、または Agent が `✓ done` のセッションの全ペインを一括で閉じます。
設定 [`auto_reclaim_merged_sessions`](05-settings.md#設定項目) に分単位の猶予を入れると、merged 検知後に同じ回収を自動実行します。
どちらも running / waiting の Agent と dirty worktree は対象外で、`session remove` や会話履歴削除までは行いません。
誤って閉じた場合も、次に `agent` を開けば CLI の再開機構で会話を復帰できます（Claude は `--continue`、Codex / codex-fugu は
`resume --last`、Gemini は `-r latest`、Antigravity は `-c`。詳細は [3.2 TUI 内コマンド#agent](03-commands/02-tui.md#agent)）。

### Agent フックによる状態報告

`claude` / `codex` で起動した Agent が「起動直後（ready）」「稼働中（running）」「入力待ち（waiting）」「ターン完了（ended）」「プロセス終了（exited）」のどれかを正確に判定するため、usagi は
起動コマンドにライフサイクルフックを差し込みます（MCP サーバや system prompt と同様、起動時に
インラインで渡す。Claude は `--settings`、Codex は `-c hooks.<Event>` 設定上書き）。フックは Agent 自身の状態遷移ごとに `usagi agent-phase <phase>` を実行し、対象 worktree
の phase を記録します。フックの payload は Claude / Codex とも同じ形（stdin の JSON に `cwd` と `source` を含む）なので、`usagi agent-phase` は CLI ごとの分岐なしに動きます。

| フックイベント | 記録する phase | 意味 |
|---|---|---|
| `SessionStart` | `ready` | 起動・再開直後＝プロンプト未投入の待機（ただしターン中の再開は例外、下記） |
| `UserPromptSubmit` | `running` | プロンプト送信＝ターン開始 |
| `PreToolUse` | `running` | ツール実行直前＝ターン中に稼働している（下記） |
| `PostToolUse` | `running` | ツール実行直後＝ターン中に稼働している（下記） |
| `Notification` | `waiting` | ターン中に**ユーザーの入力・許可を待って**停止（質問・ツール承認） |
| `PermissionRequest` | `waiting` | **ツール使用の許可プロンプト**が出た（下記） |
| `Stop` | `ended` | **ターン完了**＝Agent の実行が終わった |
| `SessionEnd` | `exited` | Agent プロセス終了（素のシェルは残る） |

- フックは payload を stdin で受け取り、usagi はそこから `cwd`（Agent を起動した worktree）を読んで対象を特定
  します。phase は `~/.usagi/agent-state/` 配下の worktree 別ファイルに記録され、ホーム画面の監視スレッドが
  読み取って左ペインの `☾ ready` / `▶ running` / `◆ waiting` / `✓ done` を駆動します（描画と検知の詳細は
  [design/home/04-keys.md](design/home/04-keys.md#使用中-agent-の表示入力待ちの検知と通知)）。
- `SessionStart` は起動・再開だけでなく**コンテキストのコンパクション後**にも発火します。自動コンパクションは**ターンの途中**でも起こり、その後 Agent は新たな `UserPromptSubmit` なしに作業を続けます。これを一律 `ready` にすると、稼働中のセッションが次の `Stop` まで `☾ ready` のまま固まってしまうため、usagi は次のいずれかに当たる `SessionStart` では **phase を書き換えず現状を維持**します（ターン中なら `running` のまま、待機中ならそのまま）。
  - payload の `source` が `compact`（明示的なコンパクション後の再開）。
  - 記録済みの phase が `running` / `waiting`（＝ターンの途中）。新規 spawn のたびに phase ファイルはクリアされる（[data/01-global.md](data/01-global.md) 参照）ため、**真の起動なら記録済み phase は無い**。途中にもかかわらず `ready` が来たのはターン中の再開（`source` を伴わないコンパクションや、`source` を読めなかった payload を含む）であり、`source: compact` の判定だけでは取りこぼすケースもこの条件で守られます。
- ツール使用の許可プロンプトも入力待ちですが、`Notification` はユーザーが**離席している**ときにしか発火しないため、それだけでは見ているセッションの許可待ちを取りこぼします。専用の `PermissionRequest` フックを `waiting` に割り当て、**プロンプト表示の直前に・フォーカス中でも**確実に `◆ waiting` へ遷移させます（観測のみで許可可否の判定には介入しません）。
- `Notification` はターン中の入力待ちだけでなく、**ターン完了後にプロンプト待ちでアイドルになったとき**にも発火します。この通知は `Stop`（`ended`）の**後**に届くため、そのまま `waiting` を記録すると完了済みセッションの `✓ done` が `◆ waiting` に巻き戻ってしまいます。そこで記録済み phase が `ended` / `exited` のときは `Notification` → `waiting` を**書き換えず維持**します。真の（ターン中の）入力待ちは直前に必ず `UserPromptSubmit` → `running` を挟むため記録済み phase は `running` であり、このガードで取りこぼすことはありません（usagi のモデルでも「ターン完了」は `done` であって `waiting` ではありません）。Codex は `Notification` / `SessionEnd` フックを持たないためこの巻き戻りは起きません。
- `exited` は process liveness の終端であり、ターンの成功を保証しません。Unix では usagi が共通の Agent 起動コマンドを shell wrapper で包み、CLI に `SessionEnd` フックが無い場合も command 終了時に `exited` を記録して元の exit status を保持します。オーケストレータは `exited` を成功イベントとして扱わず、issue / PR / CI を確認して再投入またはエスカレーションします。
- `running` を駆動するのは `UserPromptSubmit` だけではありません。**ターン中のツール呼び出し（`PreToolUse` / `PostToolUse`）も `running` に割り当て**ます。これは `◆ waiting` への貼り付きを解消するためです。ユーザーが質問に答えたり許可を承認したりして Agent が作業を再開しても、新たな `UserPromptSubmit` は発火しないので、`waiting` のままでは「実際は稼働中なのに `◆ waiting`」になってしまいます。再開後の最初のツール呼び出しで `PreToolUse` / `PostToolUse` が発火し、セッションを `▶ running` へ引き戻します。これらのフックは**ターンの途中でしか発火しない**ため、アイドル中のセッションを誤って `running` にすることはありません。
- 割り当てを見送ったフック: `SubagentStop`（サブエージェントの終了は本体ターンの終了ではない。本体は作業を続けており、`Task` ツールの `PostToolUse` が `running` を保つ）、`PreCompact` / `PostCompact`（コンパクションは上記の `SessionStart` ガードで処理され、再開後のツール呼び出しが改めて `running` を主張する）。
- `--settings` は**ユーザー自身の設定に追加マージ**されるため、既存の Claude 設定を壊しません。
- **Codex** も同じ仕組みで phase を報告します。上表のうち Codex が持つイベントは `SessionStart` / `UserPromptSubmit` / `PreToolUse` / `PostToolUse` / `PermissionRequest` / `Stop` で、`ready` / `running` / `waiting` / `ended` の割り当ては Claude と同一です（Codex には `Notification` / `SessionEnd` イベントが無く、`ended` は `Stop`、process 終了時の `exited` は上記の shell wrapper が担います）。`SessionStart` の `source`（`startup` / `resume` / `clear` / `compact`）も Claude と同じ値なので、コンパクションガードもそのまま機能します。Codex のフックは**信頼されていない command フック**として扱われ既定では実行前に承認を求めるため、usagi は `--dangerously-bypass-hook-trust` を付けて起動します（フックが実行するのは usagi 自身のみ）。対話起動は `--sandbox workspace-write --ask-for-approval on-request` も付け、worktree 内の自動実行を許しつつ、サンドボックス外へのエスカレーションだけ許可待ちにします。あわせて Codex の `sandbox_workspace_write.writable_roots` には `~/.usagi`（グローバルなキュー・phase など）に加え、セッション worktree 起動時は親ワークスペースの `<repo>/.usagi`（`state.json` と sibling session 作成先）と Git 共通ディレクトリを追加します。これにより、セッション内 Agent から MCP `session_create` / `session_delegate_issue` を呼んでも、`.usagi/.lock` の取得や sibling worktree 作成が Codex サンドボックスに阻まれません。usagi が注入する MCP サーバは `default_tools_approval_mode = "approve"` にし、MCP tool 呼び出しごとの確認は省きます。
- フックを持たない Agent（`gemini` など）はこの仕組みの対象外で、入力待ちは従来のターミナルベルで推定します。
- フック・MCP サーバが呼び戻す `usagi` は、`$PATH` 上の名前ではなく **usagi 自身の実行ファイルの絶対パス**
  （`std::env::current_exe()` で解決）を埋め込みます。これにより、インストール済みでも `cargo run` のように
  ビルド成果物（`target/debug/usagi`）を直接起動した場合でも、`usagi mcp` / `usagi agent-phase` が
  `command not found` にならず解決できます（パスが取得できない場合のみ素の名前 `usagi` にフォールバック）。

### worktree への閉じ込め（メインリポジトリ保護）

セッション worktree は**メインリポジトリの内側**（`<repo>/.usagi/sessions/<name>/`）に置かれるため、
リポジトリルートや別セッションの worktree がディスク上で 1 つ上の階層に並びます。Agent が `<repo>/src/...`
を編集したり親リポジトリへ `cd` したりすると、意図したセッションとは別のツリーを触ってしまいます。usagi は
これを 2 段階で防ぎます。

| 段 | 仕組み | 対象 | 効果 |
|---|---|---|---|
| ソフト | 「作業はこの worktree 配下だけで完結させ、親のメインリポジトリには触れない」旨を Agent に伝える。system prompt を持つ CLI は system prompt（`--append-system-prompt` ／ Codex の `developer_instructions`）で、持たない CLI は**開始プロンプト**（`-i` ／ headless の `-p`）の先頭にこの指示を置き、キュー済みプロンプトはその後ろに続ける | 全 CLI（Claude / Codex は system prompt、Gemini / Antigravity は開始プロンプト） | Agent に意図を伝える指示。強制力はない |
| ハード | `PreToolUse` フックに `usagi guard-workspace` を差し込み、ツール呼び出しを**拒否**する。判定は Agent の `cwd` によって 2 モードに分岐する（下記） | Claude | 指示を破った変更を実際にブロックする |

ハード側（`guard-workspace`）は `PreToolUse` の payload（stdin の JSON）から `cwd` を読み、それが
`.usagi/sessions/<name>/` 配下か（＝セッションの worktree か、pre-commit フックの命名規則免除と同じ判定基準）で
モードを選びます。

- **session モード**（`cwd` がセッション worktree の中）: payload の `tool_input.file_path`（ツールが触れようとする
  パス）を worktree 基準で正規化（`.` / `..` を解決）し、worktree の外に出る場合だけ拒否します。worktree 内・
  パスを持たないツール（`Bash` / `Grep` など）・解釈できない payload は素通しします。セッションは自分の worktree の
  中を自由に編集できます。
- **root モード**（`cwd` がワークスペースルート＝ `.usagi/sessions/` 配下でない。コーディネータの行）: root 行は
  リポジトリを一切変更しないため、閉じ込め（cwd == repo ルートなので「外」判定が働かない）に代えてより強く拒否
  します。
  - **ファイル書き込み系ツール**（`Edit` / `Write` / `MultiEdit` / `NotebookEdit`）を**パスに依らず**すべて拒否。
  - `Bash` のうち**リポジトリを変更する git**（`commit` / `add` / `push` / `merge` / `rebase` / `checkout -b` /
    `worktree add` など）を拒否。判定は読み取り系 git（`status` / `log` / `diff` / `show` など）の**許可リスト**で行い、
    それ以外の git サブコマンドは変更系とみなして拒否する（未知・曖昧な git を素通しさせない安全側）。git を含まない
    コマンドは素通しする。
  - 変更は root 行では行わず、セッションの worktree に委譲します。

拒否時は Claude の `PreToolUse` 契約どおり `permissionDecision: "deny"` を stdout に返します（理由も添える）。
許可する場合は何も出力せず、Claude の通常の許可フローに委ねます。

- `guard-workspace` は状態報告の `agent-phase` と同じ `PreToolUse` 配列に並べて差し込みます。Claude は同一イベントの
  フックをすべて実行し、いずれかが拒否すればツールはブロックされるため、状態報告と保護が両立します。

## ペインの復旧

ペインの復旧は、終了時に保存した TUI 側の**ペイン構成**（どのセッションにどのタブが開いていたか）を次回起動時にバックグラウンドで呼び戻す機能です。`restore_panes_enabled`（既定 ON）で切り替えます。

- **保存**: 各セッションのペイン種別、agent ペインの CLI、タブの順序とアクティブ位置を worktree 別のスナップショットに記録します。
- **復旧**: terminal ペインはシェルを開き直し、agent ペインは記録された CLI を前回の会話があればその続きから起動します。
- **終了**: TUI を終了すると PTY と agent は終了します。設定を OFF にすると保存・復旧を行わず、常に新しい状態で起動します。

### 復帰フォーカス（いた場所の復元）

ペインの復旧が「どのペインが開いていたか」を呼び戻すのに対し、**復帰フォーカス**は「終了時にユーザーが
*どこにいたか*」を呼び戻します。同じ [`restore_panes_enabled`](05-settings.md#設定項目) で一括制御します
（両者で 1 つの「セッション状態を復元する」機能）。

- **保存**: 終了が確定した時（quit 確認モーダルの承認、即時 Ctrl-C、`:quit`）に、カーソルがあったセッションと
  その**エンゲージメント段階**（選択 / 集中 / 没入）をワークスペース別のスナップショットに記録します（保存
  フォーマットは [data/01-global.md#resume-closeup復帰フォーカススナップショット](data/01-global.md#resume-closeup復帰フォーカススナップショット)）。
  没入中の終了（既定の `prefix` 方式なら `Ctrl-O q`、`alt` 方式なら `Alt-q`）は確認モーダルへ抜ける際に集中へ降格するため、降格前に「没入だった」ことを記録しておきます。
- **復旧**: 起動時、ペインの復旧が済んだ後に読み出し、記録された段階へ戻します。選択ならカーソルをそのセッションへ、
  集中ならそのセッションを集中に、没入ならイベントループの初回パスで自動的に attach します（このときペインは
  既に復旧済みなので生きており、attach できます）。
- 復旧は**ベストエフォート**です。記録されたセッションが既に消えている（`session remove` された）場合は何も
  復元せず、既定の選択で起動します。設定を OFF にすると保存も復旧も行いません。

## キュー済みプロンプトの自動起動

コーディネータ役のエージェントが MCP `session_delegate_issue` / `session_delegate_brief`（または `session_prompt` の queue チャネル）で
issue を新しいセッションへ委譲すると、そのプロンプトは[起動時キュー](03-commands/03-mcp.md#session_prompt-の挙動)
（`~/.usagi/agent-prompts/`）に積まれます。同時に TUI が利用する durable start request を
`~/.usagi/agent-start-requests/` へ publish します。**キュー済みプロンプトの自動起動**は TUI がこの request を検知したら、
対象セッションの agent ペインを**バックグラウンドで自動 spawn** して着手させる機能で、設定
[`autostart_queued_prompts`](05-settings.md#設定項目)（既定 ON）で切り替えます。自動起動の同時稼働数は
[`autostart_queued_prompt_limit`](05-settings.md#設定項目)（既定 4）で制御します。人がそのセッションのペインを開く
までエージェントが走り出さない、という自律オーケストレーションのギャップを埋めます。自動起動は TUI の稼働中に行われます。

- **start request は launch 設定を固定します**。prompt publish 時点の authoritative `SessionAgent`（CLI / model）と
  generation を request に保存するため、claim 後に別 prompt が `state.json` を更新しても先行 request の launch pair は
  後続世代へすり替わりません。claim は store lock 下の CAS で `queued → claimed(lease)` と進み、daemon と TUI が同じ
  request を同時に spawn できません。lease 中の別 consumer は queue を取り出さず、期限切れだけを再取得します。
- **TUI が consumer です**。グローバルな同時実行上限に空きがある tick だけ claim し、保存済み CLI / model、workspace
  env、MCP/phase wiring を解決して TUI 所有の PTY を spawn します。terminal registry を保存してから初期 prompt を PTY へ
  書き込み、成功後に request を `running(terminal id)` へ commit します。失敗は最大 5 回まで queue に戻し、上限後は
  dead-letter として保持します。`auto` fallback request で同じ worktree の Agent terminal が既にあれば、新規 spawn せず
  その terminal へ配送します。明示 `mode=queue` は既存 Agent へ配送しません。

- **spawn の仕組み**は[ペインの復旧](#ペインの復旧)を流用します。ライブペインを持たないセッションに対し、
  記録された agent ペインではなく agent CLI を起動し、起動後に**キュー済みプロンプトを stdin へ送って最初のメッセージ**にします
  （対象 worktree に前回の会話が残っていれば復旧と同様に続きから再開）。attach しないため画面のフォーカスは
  奪いませんが、監視スレッドが拾うので左ペインのバッジ（`▶ running` / `◆ waiting` / `✓ done`）は動きます。
  キュー済みプロンプト本体は shell の argv に埋め込まないため、複数 prompt の集約で OS の argv 上限へ近づきません。
  ローカル設定の `env`（`op://...`）解決は復旧と同じく workspace root 単位で結果を共有します。
  **セッションに `agent`（CLI / モデル）の指定があればここで適用されます**——`session_delegate_issue(agent_cli, model)` /
  `session_delegate_brief(agent_cli, model)` で
  委譲したセッションは、この自動 spawn で**指定 CLI・指定モデル**で起動します（無指定ならワークスペースの実効 `agent_cli`
  と各 CLI の既定モデル）。queue claim 後に `state.json` を読み直すため、MCP が上書きを保存した直後で HomeState の
  反映が 1 tick 遅れていても古い CLI / model で起動しません。これがセッション単位のモデル指定が最も効く経路です。
- **起動時キューとライブキューの両方**を対象にします。ライブペインを持たないセッションでは、
  [起動時キュー](03-commands/03-mcp.md#session_prompt-の挙動)（`agent-prompts/`）だけでなく
  [ライブキュー](03-commands/03-mcp.md#session_prompt-の挙動)（`agent-live-prompts/`）に滞留したプロンプトも拾って
  最初のメッセージに畳み込みます。ライブキューは本来「起動中のペインへ流し込む」チャネルですが、`session_prompt`
  の**明示 `mode="live"`** はライブペインの有無を見ずに常にライブキューへ積むため、**ペインが無いセッションに
  live 送信**するとそこに滞留します（`auto` は [live-pane マーカー](data/01-global.md#agent-live-panes)で消費者の有無を
  正しく判定するため、ペインが無いのにライブキューへ振り分けることはありません）。ペインが無ければ流し込み先が無く
  滞留し続けるので、TUI 側の権威的なペイン生存判定（PTY の生死）で「ペイン無し」と分かるこのセッションに対しては、
  ライブキューのプロンプトも自動 spawn の対象にして取りこぼしを防ぎます。
- **キューと batch には上限があります**。`session_prompt` の 1 prompt は 128 KiB まで、live queue は worktree ごとに
  64 件 / 512 KiB まで、1 回の live delivery batch は 4 件 / 256 KiB までです。上限超過は MCP tool error として返り、
  accepted な prompt は silent drop しません。ライブキューは item ファイル形式で追記されるため、append のたびに
  全件 JSON を rewrite しません（保存形式は [データ仕様](data/01-global.md#agent-live-prompts) が正本）。
- **同時稼働上限**は agent phase と dispatch 予約で数えます。`running` / `waiting` は枠を占有し、`ended` / `exited` / `ready` / `none`
  は空きとして扱います。さらに、自動起動を dispatch した直後から watcher が phase または pane exit を観測するまでは、
  worktree 単位の予約も枠を占有します。これにより、起動直後 pass と次の event-loop pass、または UI tick と watcher tick の間でも
  上限を超えません。pane 登録前の env 解決・provision は 120 秒で timeout して late spawn を破棄・再queueし、登録後に
  phase を報告しない CLI の予約は 30 秒で解放します。上限に達しているときは
  候補セッションの queue を取り出さず、そのまま残します。spawn する枠が空いた次回走査で、人手なしに自動起動します。
- **検知の契機**は 2 つです。(1) **起動時**: TUI が起動していない間にキューされたプロンプト（例: 別プロセスの
  エージェントが委譲したもの）を、次回起動時にペインの復旧・復帰フォーカスを済ませた後で拾って自動 spawn します。
  (2) **稼働中**: 定期的にキューを走査し、TUI 稼働中に委譲されたプロンプトを人の操作なしで拾います
  （両キューが空の間はディレクトリ一覧の確認だけで済ませます）。走査は**選択 / 集中のイベントループ**だけでなく、
  **没入（Attached）中のペインループ**でも同じ間隔で回ります。コーディネータ役のエージェントは自分が没入している
  ペインの中から `session_delegate_issue` / `session_prompt` を発行するのが常なので、没入中も走査することで、`Ctrl-O` で
  選択へ戻らなくても委譲先の agent が自動起動します（没入中はイベントループが止まるため、この走査を欠くと戻るまで
  起動しませんでした）。
- 既に **live agent ペイン**を持つセッションは新規 spawn しません。明示 `session_prompt(mode=queue)` の prompt は既存
  Agent へ渡さず、次の fresh launch まで起動時キューに残します。一方、既存 Agent pane は TUI を閉じても生存しますが、
  TUI 不在中は live marker が無いため `session_prompt(auto)` が起動時キューを選びます。この `auto` のフォールバック記録
  だけは、次回 TUI が既存 Agent pane を持つと（典型的にはその 既存 Agent pane の復旧後）、監視スレッドが
  ライブキューより先に引き渡して fresh launch を待たず自動着手させます。監視上 `running` / `waiting` の間は待ち、
  `ready` / `ended` または phase-less の tick に配送します。`exited` は bare shell への PTY write が成功しても Agent へ
  届かないため配送せず、queue を保持します。phase 非対応 Agent と phase check 後の状態変化は判別できないため、
  その境界は通常の live prompt と同じ best-effort です。引き渡し
  要求は冪等で、input handle への送信が失敗した prompt は retry/backoff 情報を保って起動時キューへ戻します。
  TUI 所有の PTY では durable prompt input に request id を付け、daemon が PTY へ書き込んだ後の `InputResult` ACK を
  待ちます。missing terminal、PTY write failure、ACK timeout は失敗として復元します。timeout は書き込み済みで応答だけ
  失われた可能性を区別できないため、再試行は at-least-once です。続けて送る `Kill` の ACK も確認できなければ ownership
  不明として同じ TUI run の unattended launch を停止し、孤児 Agent と自動再試行の二重起動を防ぎます。
  plain terminal だけが live なセッションには Agent が存在しないため、terminal を残したまま Agent ペインを追加で自動 spawn
  します。Agent tab が閉じた時点でその worktree の stale phase / monitor 枠を解除するため、以前の `running` / `waiting` が
  terminal-only session の同時実行枠を占有し続けません。反対に worktree-scoped な `exited` phase に複数 Agent pane がある場合は
  終了元を特定できないため自動 kill せず、その TUI run の自動起動を停止して error log へ人の解決を記録します。stale pane を
  解決・close した後に TUI を再起動すると再評価します。既存 Agent への引き渡しは新規 agent 枠を使わず、同時実行上限に達していても進みます
  （[`session_prompt` の挙動](03-commands/03-mcp.md#session_prompt-の挙動)）。
- 起動は**ベストエフォート**です。プロンプトの取り出しは 1 回限り（one-shot）で、spawn に失敗した場合は元のチャネルへ
  積み直して後続の契機・人の操作に委ねます。自動起動の失敗は launch/live の origin ごとに指数 backoff（30 秒から最大 15 分）で再試行し、
  5 回失敗すると dead-letter 状態にして毎 tick の再 spawn を止めます。dead-letter の prompt は削除せず、最後の error と
  attempt 数を `agent-prompts/` または `agent-live-prompts/<hash>/meta.json` に保持します。launch 側の古い dead record は
  独立した新しい live work を塞がず、live 側の失敗を launch の dead state に合流させません。既存 Agent pane が復帰した場合の
  live delivery は live spawn の backoff/dead-letter を迂回して配送し、成功した consumer 経路として retry state を解除します。
  設定を OFF にすると自動 spawn を一切行わず、上記の「次のフレッシュ
  起動時に消費」へ戻ります。
