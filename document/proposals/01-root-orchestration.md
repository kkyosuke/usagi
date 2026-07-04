# 提案: 自律オーケストレーション運用モデル

> [設計提案の目次](README.md) ｜ [ドキュメント目次](../README.md)

これは**設計提案**であり、現時点では未実装の運用モデルを記述する（[proposals とは](README.md)）。
実装は Epic **#105** とその子 issue（**#106–#112**）で追跡する。実装済みの正本は
[04-orchestration.md](../04-orchestration.md) / [.agents/workflow.md](../../.agents/workflow.md) /
[03-commands/03-mcp.md](../03-commands/03-mcp.md) 側にあり、機構が確定したら本提案の内容はそちらへ畳み込む
（#112）。

## 目次

- [ねらいと 3 原則](#ねらいと-3-原則)
- [目標運用フロー](#目標運用フロー)
- [root と session の責務分界](#root-と-session-の責務分界)
- [論点 1: issue の起源問題](#論点-1-issue-の起源問題)
- [論点 2: ブートストラップ（未コミット issue がブランチに乗らない）](#論点-2-ブートストラップ未コミット-issue-がブランチに乗らない)
- [論点 3: status ライフサイクル](#論点-3-status-ライフサイクル)
- [論点 4: ガードレール（実効性の担保）](#論点-4-ガードレール実効性の担保)
- [必要な機構変更（実装 issue）](#必要な機構変更実装-issue)
- [既存ドキュメントとの差分](#既存ドキュメントとの差分)
- [既存 issue との噛み合い](#既存-issue-との噛み合い)

## ねらいと 3 原則

usagi の自律オーケストレーションを、次の 3 原則が**技術的に担保された**運用モデルとして確立する。

1. **root（リポジトリルートで動くコーディネータ）はオーケストレーションのみ**を行う。許可される操作は
   issue の選択・順序付け、session の作成/委譲（`session_create` / `session_delegate_issue` /
   `session_delegate_brief` / `session_prompt`）、進捗ポーリング（`session_status` / `session_pr`）、
   完了 session の除去（`session_remove`）、次タスクの投入。
2. **root は git 追跡下のリポジトリを一切変更しない**。issue の作成・更新（status・本文）、ドキュメント編集、
   コード編集、`main` へのコミット・PR は root では行わない。
3. **リポジトリに変更が入りうる作業（調査→issue 化・実装・修正・ドキュメント更新）は必ず session の中で行う**。
   session が worktree（ブランチ）で変更し、PR で `main` に反映する。

この 3 原則が成立する土台は、既に MCP に入っている routing の非対称性である
（[03-mcp.md#起動と登録](../03-commands/03-mcp.md#起動と登録)）。

```
usagi mcp（1 プロセス）
  ├─ issue / memory  → cwd の worktree に解決（変更はブランチに乗り PR で main へ）
  └─ session 操作     → workspace root に解決（並行 worktree 全体を管理）

root で起動:    worktree == workspace_root      （両者一致）
session で起動: worktree =  .usagi/sessions/<n>/ ≠ workspace_root
```

「root で動いているか」は **`worktree == workspace_root` の一致**で機械判定できる。これがガードレールの判定軸になる。

## 目標運用フロー

```text
                    ┌─────────────────────────── root（workspace root / ⌂ root 行）──────────────────────────┐
                    │  読む: issue_search / issue_get / issue_to_prompt / session_status / session_pr        │
                    │  起こす: session_delegate_brief（起源）/ session_delegate_issue（committed 済みを委譲）  │
                    │  片付ける: session_remove                                                              │
                    │  ✗ 書けない: issue_create/update/delete・memory_save/delete・Edit/Write・main への commit │
                    └───────────────┬───────────────────────────────────────────┬──────────────────────────┘
                                    │ (A) ブリーフ委譲                          │ (B) issue 委譲
                                    ▼                                           ▼
                       ┌─ triage session ─────────┐               ┌─ work session (issue-N) ──────┐
                       │ 調査し issue を worktree   │               │ 着手で status=in-progress      │
                       │ に起票（issue_create）     │  ── PR ──▶    │ 実装＋PR 前に status=done       │
                       │ → PR                       │   merge       │ → PR                           │
                       └───────────┬───────────────┘   ▼           └───────────┬───────────────────┘
                                   │        main の backlog に issue が現れる    │
                                   └────────────────────┬───────────────────────┘
                                                        ▼  merge で status=done が main に乗る
                              root は session_status/session_pr で merged を検知 → session_remove → 次を委譲
```

作業は**起源（A）**と**遂行（B）**の 2 経路で生まれる。どちらも root は「起こして・見て・片付ける」だけで、
リポジトリを変えるのは常に session である。

## root と session の責務分界

| 操作 | root（workspace root） | session（worktree） |
|---|---|---|
| issue の選択・順序付け | ✅（`issue_search` / `issue_get`） | —（自分のタスクに集中） |
| issue の起票・本文編集 | ❌ | ✅（トリアージ/設計 session が起票） |
| issue の `status` 更新 | ❌ | ✅（**自分の issue のみ**・自枝で） |
| session 作成・委譲・プロンプト投入 | ✅ | ✅（サブ委譲も可） |
| 進捗ポーリング（phase・git 状態・PR） | ✅ | ✅ |
| 完了 session の除去 | ✅ | —（自分は消さない） |
| コード・ドキュメント編集 | ❌ | ✅ |
| `main` へのコミット・PR | ❌ | ✅（PR 経由） |

`status` の**書き手は常に当該 issue の session だけ**、という「単一書き手」原則は既存規約
（[.agents/workflow.md](../../.agents/workflow.md)）を踏襲しつつ、後述のとおり root の書き込み禁止に合わせて拡張する。

## 論点 1: issue の起源問題

**問題**: issue ファイル（`.usagi/issues/*.md`）は git 追跡下なので、その作成自体が repo 変更になる。
原則 2 により root は issue を作れない。では作業はどこから生まれるか。

**設計判断**: **起源はトリアージ/設計 session が担う**。root は事前 issue を必要としない自由記述の**ブリーフ**を
新規 session に渡し（`session_create` + `session_prompt(mode=queue)`、この定番手順を
`session_delegate_brief` に集約 → #109）、その session が worktree 内で調査し `issue_create` で起票して PR する。
issue は session のブランチに乗り、マージで `main` の backlog に現れる。

- `session_prompt` は issue を要求しないため、**この経路は新しい中核機構を要さない**（既存の
  `session_delegate_issue` と同じ合成パターンの糖衣として `session_delegate_brief` を足すだけ）。
- 起源 session が起票した issue は、マージ後に committed な backlog として root から見える。以降の遂行は
  `session_delegate_issue` に乗る。
- **root が読む backlog は「`main` にコミット済みの issue」だけ**という単純な不変条件が保てる（未マージの
  作業中 issue は各 session のブランチに閉じる）。これは論点 2・3 の前提でもある。

> トリアージと実装を 1 つの session が続けて行うことも、トリアージ session が複数 issue に分割して起票し、
> それぞれを root が別 session に委譲することもできる（粒度は運用で選ぶ）。

## 論点 2: ブートストラップ（未コミット issue がブランチに乗らない）

**問題**: `session_delegate_issue` は `issue_to_prompt` → `session_create` の順で動くが、新 worktree は
**基点ブランチ（既定 `main`）の HEAD から**切られる。基点に未コミットの issue ファイル（例: root に作られ
`main` に未マージ）は新 worktree の枝に乗らない。プロンプト本文に issue 内容は埋め込まれるので着手はできるが、
その session は**自分の worktree に issue ファイルが無い**ため `status` を `done` にできず、あるいは新規ファイルを
作って番号が二重化する。#104 が踏んだのはこれ。

**設計判断**: 二段で根治する。

1. **運用で予防**（論点 1 の帰結）: issue は必ず起源 session 経由でコミット → マージされてから委譲する。
   正常フローでは基点に issue が乗っているので問題は起きない。root が issue を作らない（原則 2・#106）ことで
   「root が未コミット issue を即委譲する」という #104 の発生条件そのものが消える。
2. **ツールで検証**（#110）: `session_delegate_issue` が、委譲先 worktree の**基点コミットに issue ファイルが
   含まれるか**を検証し（基点解決は既存 `resolve_base_ref`）、含まれなければ「まだ基点にコミットされていない」と
   明示エラーで拒否する。黙ってプロンプトだけ渡さない。

- 代替案として「新 worktree に issue ファイルをコピーして初回コミットする（自動搬送）」も検討したが、
  provenance が濁り issue が二重管理になるため**非推奨**。検証（拒否）に留め、起票はあくまで session の PR で行う。

## 論点 3: status ライフサイクル

**問題**: 「`status` を書くのは当該 session だけ」の規約だが、session がマージ後に `session_remove` されると
誰も `done` にしない。root は原則 2 で status を書けない。#104 は #615 でマージ済みなのに `todo` に取り残された。

**設計判断**: **session が生きているうちに自枝で `done` を立て、PR（マージ）で `main` に運ぶ**。これが root を
書き手にせずに済む唯一整合する経路。あわせて `in-progress` の役割を root 視点で再定義する。

| 遷移 | 誰が | どこで | いつ |
|---|---|---|---|
| `todo`（起票時） | 起源 session | 自枝 | 起票 PR に含める |
| `todo` → `in-progress` | 委譲された session | 自枝 | 着手時 |
| `in-progress` → `done` | 委譲された session | 自枝 | **PR を開く前**（実装差分と同じ PR に status 差分を載せる） |

- **`in-progress` は `main` には遅れて届く**（マージ後＝実際は完了後）。そのため root は issue ファイルの
  `in-progress` を当てにせず、**「その issue の session が生存しているか」を in-progress の実効シグナル**にする
  （`session_list` / `session_status`、命名規約 `issue-<番号>`）。root は「`main` で `todo` かつ生存 session が
  無い issue」だけを ready 候補として委譲でき、二重委譲を避けられる。
- **`done` は PR に載って `main` に届く**（#111 で `issue_to_prompt` の指示に組み込む）。root は
  `session_status.merged` / `session_pr` で取り込みを検知して `session_remove` → 次へ進む。
- **取りこぼしの是正**: 「PR 前 done」を一次対策とし、それでも残った不整合（session が done を立てずに消えた等）は
  当面**運用で吸収**する（軽量なクローズ session に `done` 化 PR を出させる／将来のマージ検知連動に委ねる）。
  root にマージ検知で status を書かせる案は原則 2 に反するため採らない。

## 論点 4: ガードレール（実効性の担保）

「root は repo を変更しない」を規約だけでなく技術で担保する。単独では穴が残るため**多層防御**にする。

| 案 | 仕組み | 塞ぐ経路 | 残る穴 | 判定軸 |
|---|---|---|---|---|
| A: MCP 書き込み拒否（#106） | 合成層 `UsagiMcpServer` が `worktree == workspace_root` のとき issue/memory の書き込み系 tool を拒否 | MCP 経由の issue/memory 変更 | Edit/Write・生 git | `worktree == workspace_root`（既存の routing seam） |
| B: guard-workspace の root モード（#107） | root 行の Agent の `PreToolUse` で Edit/Write と変更系 git を拒否 | 直接のファイル編集・`git commit` 等 | フック非経由（人手・別ツール） | cwd が `.usagi/sessions/` 配下でない |
| C: pre-commit backstop（#108） | lefthook が非セッションチェックアウトのコミットを拒否 | 人手/別ツールのコミット | フック無効化（`--no-verify`） | worktree が `.usagi/sessions/` 配下か |
| D: `main` ブランチ保護（既存） | GitHub 側で `main` 直 push 禁止＋PR 必須（既存 [enforce-pr-base.yml](../06-conventions.md#cigithub-actions)） | リモートの直更新 | ローカル作業ツリーの汚染 | サーバ側 |

**推奨: A + B を一次防壁、C をローカル backstop、D をリモート最終防壁とする多層防御**。

- **A は最小で確実**。既に存在する worktree/workspace_root の routing seam（`usagi.rs`、テスト
  `issue_and_memory_operate_on_the_worktree_not_the_workspace_root`）をそのまま判定に使え、ユニットテストで
  root=拒否 / session=許可を網羅できる。ただし MCP 経由の書き込みしか塞げない。
- **B が A の穴（Edit/Write・生 git）を埋める**。既存の worktree 閉じ込め
  （[04-orchestration.md#worktree への閉じ込め（メインリポジトリ保護）](../04-orchestration.md#worktree-への閉じ込めメインリポジトリ保護)）は
  root 行では cwd == workspace root のため「外」判定が働かず素通しする。root モードでは**パスに依らず**
  書き込み系ツールと変更系 git を拒否する。判定軸「cwd が `.usagi/sessions/` 配下か」は pre-commit の
  ブランチ名免除と同じで再利用できる。
- **C は安価な backstop**（フック非経由は塞げないので一次防壁にはしない）。**D は既にあり**、リモートの
  `main` は PR 経由に強制済み。

A と B は判定軸が別（MCP プロセス側の path 一致 / Agent の cwd）で互いに独立に効くため、一方をすり抜けても
他方が捕まえる。読み取り・整形・session 操作（`issue_search` / `issue_get` / `issue_to_prompt` /
`memory_*` の read / すべての `session_*`）は root でも許可し、オーケストレーションを妨げない。

## 必要な機構変更（実装 issue）

| # | 変更 | 層/対象 | 論点 |
|---|---|---|---|
| #106 | workspace root で issue/memory 書き込み系 tool を拒否 | `presentation/mcp/usagi.rs` | 4-A |
| #107 | guard-workspace に root モード（Edit/Write・変更系 git を拒否） | `usagi guard-workspace` / Agent 起動フック | 4-B |
| #108 | pre-commit で非セッションチェックアウトのコミットを弾く | lefthook | 4-C |
| #109 | `session_delegate_brief`（ブリーフ起点の起源フロー） | `presentation/mcp/usagi.rs` | 1 |
| #110 | `session_delegate_issue` の基点コミット検証 | `presentation/mcp/usagi.rs` | 2 |
| #111 | `issue_to_prompt` の status 指示（着手=in-progress・PR 前=done） | `usecase/issue` の prompt 整形 | 3 |
| #112 | 正本ドキュメントへ反映し proposal を畳む | `document/` / `.agents/workflow.md` | 全 |

いずれも既存の合成パターン・routing seam・フック機構の上に乗る**追加**で、新しい常駐機構や外部依存は要らない。

## 既存ドキュメントとの差分

- [.agents/workflow.md](../../.agents/workflow.md) は現行「`main` で root が行うのは issue の**定義**
  （作成・本文編集）のコミットと delegate だけで、`status` には触れない」とする。本モデルでは
  **root は issue の定義もしない**（起源はトリアージ session）。この一文を #112 で改訂する。
- [03-mcp.md](../03-commands/03-mcp.md) の tool 一覧・`session_delegate_issue` の挙動に、root での書き込み拒否
  （#106）・`session_delegate_brief`（#109）・基点検証（#110）・`issue_to_prompt` の status 指示（#111）を反映する。
- [04-orchestration.md](../04-orchestration.md) に運用モデルの横断ナラティブを正本として追記し、閉じ込め節に
  root モード（#107）を足す。

## 既存 issue との噛み合い

| issue | 役割 | 本モデルとの関係 |
|---|---|---|
| #100（done） | `session_status`（agent phase・git 状態・PR 状態の公開） | 論点 3 の完了検知（`merged`）と論点 1/2 の生存 session 判定の**基盤**。既に実装済みで、本モデルはこの上に乗る |
| #101（着手中） | root への push 型完了報告 | 完了検知の**低遅延化**。本モデルはポーリング（#100）で閉じるが、#101 が入れば `session_remove`→次委譲のループが即応になる。補完関係 |
| #99 | session 単位の agent CLI・モデル指定 | root が「軽いトリアージは小モデル、重い実装は大モデル」とタスク別に振り分ける手段。`session_delegate_brief` / `session_delegate_issue` に `agent_cli` / `model` を渡す拡張と自然に噛み合う |
| #104 | MCP 作成セッションのサイドバー即時反映 | 委譲した session が即座に一覧へ現れる前提。本 Epic のガードレール（#106）と起源フロー（#109）は #104 の再発条件（root の未コミット issue 即委譲）自体を解消する |
