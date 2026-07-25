---
number: 538
title: feat(core): env を global / workspace の 2 層設定にして子プロセスへ注入する
status: done
priority: high
labels: [core, daemon, tui, settings, environment]
dependson: []
related: [399, 244]
created_at: 2026-07-25T00:00:00.000000+00:00
updated_at: 2026-07-25T00:00:00.000000+00:00
---

## 背景

v1 は環境変数を **global（`~/.usagi/settings.json` の `env`）と workspace（`<repo>/.usagi/settings.json` の
`env`）の 2 層**で持ち、workspace 優先でマージし、`op://` reference を `op read` で解決して agent / terminal
の pane 起動時に子プロセスへ注入していた（`v1/document/05-settings.md`）。

v2 の env は #399 で Overview の `env` overlay として本番配線されたが、次の 3 点が v1 と異なっていた。

- 保存先が repository の `state.json` で、スコープが **workspace root / session** だった（global が無い）。
- **どこにも注入されていなかった**（editor で編集できるだけで、pane の環境には影響しない）。
- secret reference の解決経路が無かった。

## 目的

v1 を参考に、env を **global（全 workspace 共通）と workspace（その repository 固有）の 2 層設定**として
実装し直し、**workspace を編集するときは global に何が設定されているかが見える**ようにする。さらに、実効
env を daemon が Agent / terminal の子プロセスへ実際に注入する。

## スコープ

- **core domain**: `Settings::env` / `LocalSettings::env`（`EnvBindings`）と binding 語彙
  （`is_valid_env_name` / `is_secret_reference` / `parse_env_bindings` / `format_env_bindings` /
  `valid_bindings`）。`Settings::with_local` が env を **累積マージ**（同名は workspace 勝ち）する。
  Config 画面の保存が workspace の env を巻き込まないよう `LocalSettings::with_config` を通す。
- **core usecase** (`usecase::env`): `EnvScope`（Global / Workspace）、`SecretResolver` port、literal は
  そのまま・`op://` だけ resolver 経由という解決方針、失敗 binding の `ResolvedEnvironment::failures` 報告。
- **core infrastructure** (`infrastructure::env_resolver`): binding ごとの並列解決と、実 subprocess の
  1Password CLI（`op read --no-newline`、30 秒 deadline）。
- **TUI**: `env` overlay を target scope から **settings scope** へ移し、workspace 編集時に global の
  binding を read-only 併記（同名は上書き済み表示）。`NAME=value` の入力行、空値で削除、空入力で保存、
  `Tab` でスコープ切替。`env [workspace|global]` の引数、saving 中の二重送信ガード、失敗時の入力保持。
- **合成ルート**: settings 2 ファイルを正本とする `SettingsEnvironmentStore`（scope 単位のロック付き書き込み）と、
  daemon 側の `UserEnvironment`（2 ファイル読み → マージ → 解決 → workspace 単位の設定内容キャッシュ）。
  Agent adapter の spawn provision と terminal profile へ注入し、durable snapshot には**名前だけ**載せる。
- **ドキュメント**: `document/09-env.md`（env 設定の正本）を新設し、`document/03-tui.md` の env editor 節と
  目次を更新する。

## 対象外

- `state.json` の per-session env（本 issue で廃止。session 固有の env が必要になったら別 issue で 3 層化を検討する）。
- IPC への環境変数 field 追加（daemon が settings を直接読む方針を維持する）。
- 稼働中 pane への再注入（反映は次に開く pane から）。
- 1Password service-account token の keychain 保存（`usagi op login` 相当）。

## 受け入れ条件

- global / workspace の双方に env を保存でき、実効 env は global にワークスペースを重ねた結果（同名は workspace 勝ち）になる。
- workspace の editor で global の binding が read-only で見え、同名は上書き済みと分かる。
- Agent / terminal の pane を新しく開くと、実効 env が子プロセスの環境に入る。
- `op://` の値だけが `op read` で解決され、解決できない binding はその変数だけ落ちて error ログに残る。
- Config 画面の保存で workspace の env が失われない。
- coverage 100% を維持し、`#[coverage(off)]` は実 subprocess と合成ルートの実 IO に限る。

## 検証

- `cargo test -p usagi-core` / `-p usagi-tui` / `--bin usagi`、`cargo clippy --workspace --all-targets -- -D warnings`、
  Markdown link check。full gate は PR CI。
