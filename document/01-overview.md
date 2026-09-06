# 1. プロジェクト概要

> [ドキュメント目次](README.md) ｜ 次へ → [2. アーキテクチャ](02-architecture.md)

## 目次

- [usagi とは](#usagi-とは)
- [設計方針](#設計方針)
- [現在の実装状態](#現在の実装状態)
- [入口面](#入口面)
  - [CLI](#cli)
  - [daemon command](#daemon-command)
  - [session command](#session-command)
- [実行モデル](#実行モデル)
- [仕様の読み分け](#仕様の読み分け)

## usagi とは

`usagi` はセッション・worktree オーケストレータである。リポジトリごとに隔離された
worktree（セッション）を作り、複数の AI エージェント・シェルを並行して走らせ、
issue の委譲から PR の作成・マージまでのループを回す。

## 設計方針

PTY 所有を daemon に移し、TUI は daemon が所有する端末に attach するクライアントとして構成する。
コードの構成は [2. アーキテクチャ](02-architecture.md) を正本とする。

## 現在の実装状態

usagi は 1 つの daemon process で複数 workspace を tenant として扱い、workspace root と各 session の
Agent / generic terminal PTY を所有する。TUI、CLI、MCP は daemon の client であり、session 一覧、実行中
runtime、PR、terminal stream を daemon の snapshot と event から表示する。TUI を閉じても daemon-owned
process は継続し、再接続時は durable identity によって同じ resource へ attach する。

Workspace は複数の project tab として同時に開ける。各 workspace は root scope と managed session を持ち、
session は `.usagi/sessions/<name>/` の隔離 worktree である。Claude、OpenAI Codex、Sakana AI と通常の
shell を同じ pane model で起動できる。画面、キー操作、設定 UI の正本は [3. TUI](03-tui.md) とする。

## 入口面

### CLI

以下が人間向け CLI command tree の正本である。オプションの排他・必須条件を含む parser の最終的な
実行契約は `usagi --help` と各 subcommand の `--help` が返す。表にない positional argument は受理しない。

| コマンド | 動作 |
|---|---|
| `usagi` / `usagi hop` | Welcome TUI を開く。`hop` は互換用の非表示 alias |
| `usagi open [path]` | workspace を登録して TUI で開く。省略時はカレントディレクトリ |
| `usagi config` | Global Config TUI を開く |
| `usagi doctor [--fix]` | 必要ツール、settings、既存 daemon を診断し、daemon / Agent lifecycle 以外の修復可能項目だけを修復する |
| `usagi daemon restart [--restart-agents] [--force]` | daemon を入れ替える。`--restart-agents` は同一 workspace の全 live Agent を durable な計画から exact resume し、併用時の `--force` は Running 中の中断を許可する。複数 workspace は停止前に拒否し、`--force` 単独は live runtime を破棄する |
| `usagi clean [--dry-run\|--apply [--force]]` | 孤立 workspace data、worktree、branch、process を照合する。既定は dry-run |
| `usagi update [-v]` | 最新 release、または `-v` で選択した release へ更新する |
| `usagi completion <shell>` | shell 補完スクリプトを標準出力へ生成する |
| `usagi version` / `usagi --version` | 配布 version を表示する |
| `usagi daemon [command]` | daemon process lifecycle を操作する |
| `usagi session <command>` | daemon-owned session lifecycle / resume / prompt を操作する |

`usagi mcp` と Agent integration 用 hook command は配布バイナリに含まれるが、人間向け help には表示しない。
MCP の起動、公開 tool、認証、daemon への反映経路は [7. MCP サーバ](07-mcp.md) が正本である。

### daemon command

| コマンド | 動作 |
|---|---|
| `usagi daemon` | daemon を前景で serve する |
| `usagi daemon start` | detached daemon を起動する |
| `usagi daemon status` | active daemon と保持中 tenant の状態を表示する |
| `usagi daemon retire <path> [--force]` | 指定 workspace tenant を解放する。live runtime は `--force` なしでは解放しない |
| `usagi daemon stop [--force]` | daemon を停止する。live runtime は `--force` なしでは停止しない |
| `usagi daemon restart [--restart-agents] [--force]` | daemon を入れ替える。generic live PTY は seamless rollover で維持する。live Agent は通常拒否し、`--restart-agents` で exact resume、さらに `--force` を併用すると Running 中の中断も許可する |
| `usagi daemon replace [--force]` | 現在 daemon の artifact replacement を明示的に要求する |
| `usagi daemon install-service` | macOS LaunchAgent / Linux systemd user service を登録する |
| `usagi daemon uninstall-service` | 登録済み user service を削除する |

`restart` / `replace` の handoff、force、failure contract は
[5. daemon#planned replacement](05-daemon.md#planned-replacement)を正本とする。

### session command

| コマンド | 動作 |
|---|---|
| `usagi session create <name> [--role <id>] [--base <ref>]` | managed session を作る。base は fully-qualified local / remote-tracking ref |
| `usagi session remove <name> [--force [--purge-orphan]]` | managed session の削除を daemon に要求する。診断済み integrity orphan の破棄には両 flag が必要 |
| `usagi session sleep <name>` | 再開可能で idle な Agent の process / PTY を止め、session と会話履歴を保持する |
| `usagi session resume-inventory <workspace-id>` | root / session の再開候補を列挙する |
| `usagi session resume-exact <target-json>` | inventory が返した secret-free target を完全一致で再開する |
| `usagi session setup <name> <command>` | session worktree で setup command を実行する |
| `usagi session prompt <name> <prompt>` | live Agent へ prompt を配送する。live Agent が無ければ失敗し、暗黙に queue へ退避しない |

`session` と手動起動した `mcp` は daemon が停止中なら自動起動する。未採用 repository は、その
repository root から実行した場合だけ tenant として adopt する。cwd、workspace fence、session identity の契約は
[4. daemon IPC#workspace fence](04-ipc.md#workspace-fence) と [5. daemon](05-daemon.md) を正本とする。

## 実行モデル

```text
human ── TUI / CLI ──┐
                     ├── daemon ── workspace/session state, Agent, PTY, PR
agent ── stdio MCP ──┘
```

- daemon は managed lifecycle と live runtime の単一書き手である。
- client は daemon が発行した workspace / session / worktree / terminal / operation identity を使い、名前や
  path だけから effect 対象を推測しない。
- issue は caller worktree の `.usagi/issues/`、daemon-provisioned memory は Git 追跡外の workspace store を使う
  （手動 MCP は cwd root の互換経路）。どちらも Markdown source が権威で、derived index は再構築可能である。
- settings は Global と Workspace の2層、role catalog は Global / Workspace / repository の合成で解決する。
- 非対話環境では TUI entry の1フレームを出力して終了し、Doctor は診断結果と終了 status を返す。

## 仕様の読み分け

| 知りたいこと | 正本 |
|---|---|
| クレート、依存方向、永続化、CLI dispatch の実装境界 | [2. アーキテクチャ](02-architecture.md) |
| 画面遷移、キー、pane、project tab、settings UI | [3. TUI](03-tui.md) |
| wire、identity、fence、request / event、transport | [4. daemon IPC](04-ipc.md) |
| session / terminal / Agent lifecycle、daemon generation | [5. daemon](05-daemon.md) |
| 開発・品質・ドキュメント規約 | [6. 開発規約](06-conventions.md) |
| MCP method、tool、resource、認証 | [7. MCP サーバ](07-mcp.md) |
| coverage exclusion の管理 | [8. coverage exclusion inventory](08-coverage.md) |
| 子プロセスへ渡す環境変数 | [9. 環境変数設定](09-env.md) |
| session role と prompt 合成 | [10. session role](10-session-roles.md) |

設計提案、採用理由、却下案は [proposals](proposals/README.md)、未完了作業は issue store を参照する。
proposal と issue は現在仕様の正本ではない。
