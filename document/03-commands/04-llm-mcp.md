# 3.4 ローカル LLM MCP サーバ（`usagi llm-mcp`）

> [コマンドリファレンス](README.md) ｜ ← 前へ [3.3 MCP サーバ](03-mcp.md) ｜ 次へ → [3.5 1Password MCP サーバ](05-op-mcp.md)

`usagi llm-mcp` は、ローカルで動く LLM（[Ollama](https://ollama.com) 経由）を
**MCP（Model Context Protocol）サーバ**として AI エージェントに公開するコマンドです。
クラウド Agent（Claude Code など）は、要約・命名・定型文生成・単純な変換といった
**軽量で重要度の低いタスク**をローカル LLM に委譲でき、自分（課金対象）のトークン消費を抑えられます。

## 目次

- [概要](#概要)
- [有効化（config）](#有効化config)
- [資材のインストール](#資材のインストール)
- [起動と登録](#起動と登録)
- [対応 tool 一覧](#対応-tool-一覧)
- [アーキテクチャ](#アーキテクチャ)
- [設計上の選択](#設計上の選択)

## 概要

- **トランスポート**: stdio 上の **JSON-RPC 2.0**（[issue MCP サーバ](03-mcp.md) と同じ実装）。
- **バックエンド**: `ollama run <model>` へシェルアウト。プロンプトを標準入力で渡し、標準出力を返す。
- **サーバの自動起動**: 呼び出しのたびに Ollama サーバの起動を確認し、停止していれば
  `ollama serve` をバックグラウンドで起動して接続できるまで待ちます。Homebrew で入れた `ollama` は
  サーバを常駐させず、`run` / `pull` も自動起動しないため、これがないと
  `could not connect to ollama server` で失敗します。
- **既定は無効**: usagi が勝手に有効化することはありません。下記の通り config で明示的に on にします。

## 有効化（config）

グローバル設定 `settings.json` の `local_llm` で制御します（[5. 設定](../05-settings.md) 参照）。

| キー | 既定 | 意味 |
|---|---|---|
| `local_llm.enabled` | `false` | 有効化すると Agent 起動時に `usagi-llm` サーバが wire される |
| `local_llm.model` | `qwen2.5-coder:7b` | 委譲先の Ollama モデル名 |

Config 画面（`usagi hop` → Config）から編集できます。上級者向けには、ヘルプ非表示の `usagi config --edit` でも編集できます。

- **Local LLM** 行: `ollama` ランタイムが未導入のうちは値が `Install`（緑のアクションラベル）になり、
  `Space` / `Enter` で**インストールモーダル**を開きます。モーダルで sudo パスワードを入力し `Enter` で確定すると、
  ランタイム導入をバックグラウンドで実行します（スピナー表示。モデルはここでは取得しません）。完了すると
  **on/off トグル**に変わって有効状態になり、カーソルが `Local LLM Model` 行へ移動してモデルを選べます。
- **Local LLM Model** 行: ランタイム未導入のうちは操作不可です。導入後に `Space` / `Enter` で**モデル選択モーダル**を
  開き、提供モデルを導入状態のマーカー付き（`✓ 導入済` / `⬇ 未導入`）で一覧します。導入済みモデルはそのまま採用し、
  未導入モデルはその場で `ollama pull`（sudo 不要・スピナー表示）してから採用します。

`local_llm.enabled` はプロジェクト単位の[ローカル設定](../05-settings.md#ローカル設定プロジェクト単位の上書き)でも上書きできます。

## 資材のインストール

「資材」= `ollama` 本体と選択モデルです。次のいずれからも導入できます。

- **Config 画面**（上記）。ランタイムとモデルを**別ステップ**で導入します。`Local LLM` 行の Install アクションは
  公式インストーラ（`curl -fsSL https://ollama.com/install.sh | sh`）をバックグラウンドで実行し、入力した sudo
  パスワードで事前認証してランタイム導入を非対話で進め、必要なら Ollama サーバを起動します（モデルは取得しません）。
  モデルは `Local LLM Model` 行のモデル選択モーダルで、未導入のものを選んだときに `ollama pull`（sudo 不要）で
  取得します。実行中はスピナーを表示し、TUI はブロックしません。
- **`usagi doctor --fix`**: `local_llm.enabled` が `true` のとき、`ollama` 本体（公式インストーラ）を導入し、
  必要なら Ollama サーバを起動してから、選択モデル（`ollama pull <model>`）まで一括で導入します。
  CLI 上では sudo が必要に応じて対話的にパスワードを尋ねます。`usagi doctor` は導入状況を健全性チェックとして表示します。

> 導入は「`ollama` 本体 → サーバ起動 → モデル取得」の順に進みます。サーバ起動はモデル取得が
> `could not connect to ollama server` で失敗しないための前提ステップです。Config 画面ではこのうち
> ランタイム導入（本体＋サーバ起動）とモデル取得を別々のアクションに分けています。

公式インストーラが対応しない OS（macOS / Linux 以外）では、
[公式ダウンロードページ](https://ollama.com/download) を案内します。

## 起動と登録

通常は `local_llm.enabled` を有効にすれば、`agent` 起動コマンドに自動で登録されます（下記）。
手元での確認はシェルから直接起動できます。

```bash
usagi llm-mcp --model qwen2.5-coder:7b   # stdin から JSON-RPC を読み、stdout へ応答を書く
```

有効時、Claude Code 用の `--mcp-config` には issue サーバと並んで次が追加されます。

```json
{
  "mcpServers": {
    "usagi":     { "command": "usagi", "args": ["mcp"] },
    "usagi-llm": { "command": "usagi", "args": ["llm-mcp", "--model", "qwen2.5-coder:7b"] }
  }
}
```

あわせて、軽量タスクをローカル LLM に委譲するよう促す一文がシステムプロンプトに追記されます（Claude は `--append-system-prompt`、Codex は `developer_instructions` で追記。Gemini は対応しません）。

Codex は `--mcp-config` を持たないため、同じ内容を `-c` 設定上書きで注入します（`mcp_servers.usagi` と、有効時の `mcp_servers.usagi-llm`）。

```bash
codex -c 'mcp_servers.usagi.command=usagi' -c 'mcp_servers.usagi.args=["mcp"]' \
      -c 'mcp_servers.usagi-llm.command=usagi' \
      -c 'mcp_servers.usagi-llm.args=["llm-mcp","--model","qwen2.5-coder:7b"]'
```

## 対応 tool 一覧

`tools/list` で以下の 1 tool を公開します。

| tool | 必須引数 | 任意引数 | 返り値 |
|---|---|---|---|
| `local_llm_ask` | `prompt` | `system`（先頭に付与するシステム指示） | ローカルモデルの補完テキスト |

## アーキテクチャ

```
クラウド Agent ⇄ (stdio JSON-RPC)
        │
        ▼
presentation/cli/llm_mcp.rs   … stdin ループ + Ollama バックエンド（テスト不能・カバレッジ対象外）
        │  handle_line(line) ごとに委譲
        ▼
presentation/mcp/llm.rs       … LlmMcpServer：tool 実装（JSON-RPC フレーミングは mcp/mod.rs と共有・100% テスト）
        │  LlmBackend::ask 経由
        ▼
（テスト時）FakeBackend / （本番）OllamaBackend → `ollama run <model>`
```

| モジュール | 役割 |
|---|---|
| `presentation/cli/llm_mcp.rs` | `usagi llm-mcp` のエントリ。stdin ループと `ollama` へのシェルアウト。`mcp` 同様カバレッジ対象外。 |
| `presentation/mcp/llm.rs` | `LlmMcpServer`。`McpService` を実装し `local_llm_ask` tool を提供（JSON-RPC フレーミングは `mcp/mod.rs` と共有）。`LlmBackend` トレイトでモデル呼び出しを抽象化。ユニットテストで網羅。 |
| `usecase/local_llm.rs` | `ollama` / モデルの有無判定とインストール、および Ollama サーバの起動確認・自動起動（`doctor::CommandRunner` を再利用）。 |

## 設計上の選択

- **HTTP 依存を増やさない**: Ollama の HTTP API ではなく `ollama` CLI へシェルアウトすることで、
  `reqwest` 等の追加依存を避け、usagi の「依存を最小に保つ」方針に合わせています。
- **issue MCP と同じ最小実装**: `serde_json` のみで同期的に JSON-RPC を処理し、
  テスト不能な stdin ループ・シェルアウトだけをカバレッジ対象外にしています（[03-mcp.md](03-mcp.md) と同方針）。
- **オプトイン**: 既定は無効。有効化・資材導入はすべてユーザー操作（config / `doctor --fix`）が起点です。
