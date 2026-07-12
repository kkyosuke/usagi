# 提案: 入口面（CLI / MCP）の配置と daemon を権威とする反映フロー

> [設計提案の目次](README.md) ｜ [ドキュメント目次](../README.md)

## 目次

- [要旨](#要旨)
- [背景](#背景)
- [面の責務](#面の責務)
- [ディレクトリ構成と依存](#ディレクトリ構成と依存)
- [tool / コマンドの 2 分類と経路](#tool--コマンドの-2-分類と経路)
- [反映フロー（session_create の例）](#反映フローsession_create-の例)
- [採らなかったフロー](#採らなかったフロー)
- [daemon 不在時の挙動](#daemon-不在時の挙動)
- [クリーンアーキテクチャ上の位置づけ](#クリーンアーキテクチャ上の位置づけ)
- [検討した代替案](#検討した代替案)
- [正本への畳み込み](#正本への畳み込み)

## 要旨

v2 の**実行面**は TUI / daemon の 2 つ（[02-architecture.md](../02-architecture.md)）だが、usagi には
常駐しない入口が 2 つある: 人間向けの **CLI**（`usagi issue` / `usagi status` など）と、エージェント向けの
**MCP サーバ**（`usagi mcp`）である。本提案はこの 2 つを**入口面（headless presentation）**として
第 4 のクレート `usagi-cli`（`crates/cli`）にまとめ、次の 2 原則で責務を確定する。

1. **実行と session 状態の権威は daemon ただ一つ**。CLI / MCP の session 系操作は daemon への
   IPC リクエストであり、自分では worktree 生成・prompt 配送・`state.json` 書き込みを行わない。
2. **TUI はいかなる実行経路にも入らない純クライアント**。MCP 操作の TUI への反映は、daemon が
   attach プロトコルで push する既存の `Sessions` 通知だけで完結する。

したがって「TUI から起動した agent が MCP で session を作る」フローは
**tui → agent（daemon 所有 PTY）→ mcp → daemon →（push）→ tui** となる。

## 背景

v1 では MCP はすでに「委譲＝ファイル queue に書く／実行＝TUI が拾う」で実行側と分離されていた
（[v1/document/proposals/02-daemon.md](../../v1/document/proposals/02-daemon.md#現状のプロセス境界)）。
v2 はその「実行」側を daemon に移すことが決定済みで、TUI は attach クライアントになる。
このとき未確定なのは次の 2 点である。

- `usagi <サブコマンド>`（CLI）と `usagi mcp` を**どのクレートに置くか**。
- MCP tool（特に `session_create` / `session_prompt` など実行を伴うもの）の結果を
  **daemon と TUI にどう反映するか**。

v1 は `state.json` を複数プロセスがロック付きで読み書きし、TUI の watcher が変化を拾う
**多書き手＋監視**モデルだった。v2 では daemon が常駐する前提を活かし、session 状態の書き手を
daemon に一本化して、ロック・watcher の競合処理を daemon 内の直列化に置き換える。

## 面の責務

| 面 | クレート | 責務 | 持たないもの |
|---|---|---|---|
| daemon | `usagi-daemon` | **実行の権威**。PTY 所有、セッション監視・通知、委譲 queue の消化、session 状態（`state.json`）の単一書き手、IPC サーバ | 描画・キー入力 |
| TUI | `usagi-tui` | **表示と入力**。attach プロトコルのクライアント、画面描画、キー入力の転送 | 実行の権威（spawn / autostart / 状態書き込み）。他プロセスからの依頼の中継 |
| CLI | `usagi-cli`（`cli/`） | **人間向けの入口**。引数解析と結果整形。store 系は core usecase を直接呼び、session 系は daemon の IPC クライアントになる | 常駐・PTY・独自ロジック |
| MCP | `usagi-cli`（`mcp/`） | **エージェント向けの入口**。stdio JSON-RPC の解釈と tool アダプタ。経路は CLI と同一（store 系は直呼び、session 系は IPC） | 常駐・PTY・独自ロジック。CLI の子プロセス起動 |
| 合成ルート | ルート bin | 実 IO の注入と実行面への dispatch のみ | テスト対象になるロジック |

CLI と MCP は**同じ core usecase を呼ぶ兄弟アダプタ**であり、片方がもう片方の上に載る関係ではない。
v1 で JSON 出力の SSoT を usecase 側（`view`）に置いて CLI・MCP が共用した構造を踏襲し、
共有すべきものはすべて `usagi-core` に置く。

## ディレクトリ構成と依存

> この節のクレート構成・依存・dispatch は実装済みで、正本は
> [02-architecture.md](../02-architecture.md)。本節は設計判断の経緯として残す。

```text
crates/
├── core/             # usagi-core: domain / 共有 usecase / infrastructure（IPC プロトコル型・store・git）
├── daemon/           # usagi-daemon: 実行面（サーバ側）
├── tui/              # usagi-tui: 実行面（attach クライアント側）
└── cli/              # usagi-cli: 入口面（常駐しない headless presentation）
    └── src/
        ├── lib.rs
        ├── cli/      # 人間向けサブコマンド（issue / memory / status / clean / ...）
        └── mcp/      # MCP サーバ（stdio JSON-RPC フレーミング・tool アダプタ）
```

合成ルートの dispatch は第 1 引数で決める。

| 起動 | 実行面 |
|---|---|
| `usagi`（引数なし） | `usagi-tui`（TUI） |
| `usagi daemon ...` | `usagi-daemon` |
| `usagi mcp` | `usagi-cli` の `mcp/`（stdio serve ループ） |
| その他のサブコマンド | `usagi-cli` の `cli/` |

依存は既存ルールの延長で、入口面も**他の面クレートに依存しない**。

```text
          usagi（bin, 合成ルート）
         │        │         │
         ▼        ▼         ▼
    usagi-tui  usagi-cli  usagi-daemon
         │        │         │
         └───► usagi-core ◄─┘
```

- `usagi-cli` は `usagi-core` にのみ依存する。daemon との連携は `usagi-core` の IPC プロトコル型を
  介した実行時通信だけで行う（TUI と同じ規律）。
- `usagi mcp` はエージェントが spawn する**別プロセス**である。カレントディレクトリ（session worktree か
  workspace root か）が issue / memory の書き込み先を決める v1 のセマンティクスを保つため、
  stdio プロセスとして cwd の文脈を運ぶ。

## tool / コマンドの 2 分類と経路

CLI コマンドと MCP tool は、実行を伴うかどうかで経路が 2 つに分かれる。

| 分類 | 対象 | 経路 | 理由 |
|---|---|---|---|
| store 系 | issue / memory の CRUD・検索 | cwd の `.usagi/{issues,memory}/` を core usecase で直接読み書き | git 追跡ファイルの編集であり実行を伴わない。session worktree 内ならブランチに乗って PR で `main` へ流れる（v1 と同じ）。daemon 不要 |
| session 系 | `session_create` / `session_prompt` / `session_status` / `session_remove` / `session_delegate_*` と対応 CLI | daemon への IPC リクエスト。daemon が worktree 生成・`state.json` 記録・prompt 配送・破棄を実行する | 実行（PTY・autostart）と session 状態の権威が daemon にあるため。書き手の一本化で v1 のロック・watcher 競合を排す |

prompt 配送も daemon に一本化される。

- **live 配送**: daemon が対象端末の PTY に直接書く（v1 で TUI の監視スレッドが「貼り付け → Enter」して
  いた役割の移設）。live か queue かの `auto` 判定は、v1 の live-pane マーカーに代わって daemon 自身の
  端末・attach テーブルで行う（daemon が権威なので推測が要らない）。
- **queue 配送**: durable queue に積み、daemon の consumer が消化する。

## 反映フロー（session_create の例）

TUI から起動した agent（daemon 所有 PTY 内で動作）が MCP tool で session を作るときの全体像。

```text
 TUI               daemon                agent（daemon 所有 PTY 内）   usagi mcp（agent の子）
  │ attach 済み      │                      │                            │
  │                  │                      │ tool 呼び出し               │
  │                  │                      │─── stdio JSON-RPC ───────► │
  │                  │ ◄──────── IPC: SessionCreate { name, ... } ────── │
  │                  │ worktree 生成・state 記録（daemon 内で直列化）      │
  │                  │ ─────────── IPC: 応答 { session } ──────────────► │
  │ ◄─ Sessions push │                      │ ◄── tool 結果 ──────────── │
  │ 一覧を再描画      │                      │                            │
```

- TUI への反映は daemon の **`Sessions` push**（attach プロトコルの既存メッセージ）だけで行う。
  MCP のための新しい反映機構は作らない。TUI は MCP の存在を知らない。
- TUI が閉じていても daemon が実行を完了する。次に TUI を開けば attach 時の snapshot に反映済み。
- `session_prompt` も同型: MCP → daemon（IPC）→ daemon が PTY 書き込み（live）または queue 投入
  → agent phase の変化を daemon の監視が拾い、TUI へ push。

## 採らなかったフロー

「mcp → cli → tui → agent」のように CLI や TUI を経路に挟む案は採らない。

| 観点 | 問題 |
|---|---|
| TUI 非依存 | TUI を実行経路に含めると「TUI が開いている間しか反映されない」という v1 の弱点が戻り、daemon 化の目的（[v1/document/proposals/02-daemon.md](../../v1/document/proposals/02-daemon.md#要旨)）に反する |
| インターフェース | MCP が CLI を子プロセスとして呼ぶと、型付きの usecase 呼び出しが argv / stdout の文字列越しになり、エラー伝播・テスト・スキーマ整合が劣化する。両者は同じ usecase を呼ぶ兄弟にする |
| 書き手の一本化 | CLI / MCP が `state.json` を直接書くと daemon と書き手が並立し、v1 のファイルロック・watcher 競合が復活する |

## daemon 不在時の挙動

- **session 系**: CLI / MCP は TUI と同じ方針で daemon を **autospawn** する（ユーザー・エージェントは
  daemon を意識しない）。autospawn できない環境（非 Unix など）では tool / コマンドエラーとして返し、
  「daemon を迂回して直接実行する」フォールバックは持たない（書き手の一本化を優先する）。
- **store 系**: daemon なしで動く（cwd のファイル操作のみ）。

## クリーンアーキテクチャ上の位置づけ

- `usagi-cli` の `cli/` と `mcp/` はどちらも **presentation**（引数 / JSON-RPC の解釈と結果整形だけ）。
- daemon IPC のプロトコル型とクライアントは `usagi-core` の **infrastructure**（TUI の attach クライアントと共用）。
- session 系の実行ロジック（worktree 生成・配送・直列化）は daemon 側の usecase、
  検証・整形など面をまたぐロジックは `usagi-core` の usecase に置く。
- 実 IO（stdio ループ・socket 接続）は合成ルートで束ね、入口面クレートは注入でテスト可能に保つ
  （[06-conventions.md#品質チェックリスク比例の-gate](../06-conventions.md#品質チェックリスク比例の-gate)）。

## 検討した代替案

| 代替案 | 不採用の理由 |
|---|---|
| CLI / MCP を合成ルート（ルート bin）に置く | ルートは実 IO 注入のみで `COVERAGE_IGNORE` 対象。tool アダプタ・引数解析はテスト対象のロジックであり crates 側に置く |
| MCP を `usagi-daemon` に置く（daemon が MCP を serve する） | MCP はエージェントごとに spawn される stdio の**クライアント側**プロセスで、cwd（session worktree）の文脈を運ぶ。常駐サーバに置くと issue / memory の書き込み先解決が cwd から切り離され、v1 のセマンティクスを失う |
| MCP を `usagi-core` に置く | core は両面が共有するライブラリで presentation を含めない。入口を core に入れると依存の終点が入口を持つ逆転になる |
| `crates/mcp` を `crates/cli` と別クレートに分ける | どちらも core にしか依存しない薄いアダプタで、互いの逆流リスクがなく、コンパイラで強制すべき境界がない。クレートを増やす利益が boilerplate に見合わない（肥大したら分割を再検討） |
| CLI / MCP が `state.json` を直接書き、daemon が watcher で拾う（v1 方式の継続） | 多書き手のロック・watcher 競合が残る。daemon 常駐を前提にできる v2 では、書き手を daemon に一本化する方が単純で、push による即時反映も得られる |

## 正本への畳み込み

実装が進んで挙動が確定したら、クレート構成・依存図・dispatch 表は
[02-architecture.md](../02-architecture.md) へ、IPC メッセージ・配送の仕様は該当する仕様ドキュメント
（v1 の番号体系で新設）へ畳み込み、本提案はリンクスタブ化する。実装タスクは issue ストアで追跡する。
