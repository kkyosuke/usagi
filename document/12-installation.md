# 12. インストールと更新

> [ドキュメント目次](README.md) ｜ ← 前へ [11. キーバインド](11-keybindings.md)

利用者向けの対応環境、インストール、更新、shell 補完の正本である。
公開 CLI の command tree は [1. プロジェクト概要](01-overview.md#入口面)、
各 command の最終的な構文は `usagi <command> --help` を正本とする。

## 目次

- [対応環境と必要なもの](#対応環境と必要なもの)
- [installer で導入する](#installer-で導入する)
- [ソースからビルドする](#ソースからビルドする)
- [更新](#更新)
- [shell 補完](#shell-補完)

## 対応環境と必要なもの

公開 release と installer が対応する platform は次の 3 つである。

| OS | architecture |
|---|---|
| macOS | amd64 / arm64（Apple Silicon） |
| Linux | amd64 |

daemon IPC、PTY、process・permission API が Unix の機能を使うため、Windows は対象外である。
installer は Bash、`curl`、`tar`、`sha256sum` または `shasum` を使う。

`usagi` の利用には Git が必要である。AI Agent を起動する場合は、利用する Agent の CLI も用意する。
対応する Agent 名と executable の組は
[Closeup の Agent CLI 選択](03-tui.md#closeup-の-agent-cli-選択)を正本とする。
ソースからビルドする場合だけ、`rust-toolchain.toml` が指定する Rust / Cargo も必要である。

## installer で導入する

次の installer は最新の公開 release を `~/.usagi/bin/usagi` へ導入する。

```bash
curl -fsSL https://raw.githubusercontent.com/KKyosuke/usagi/main/scripts/install.sh | bash
```

installer は archive の SHA-256 と release version artifact を検証してから binary を差し替える。
archive 構造、検証、atomic replacement の内部契約は
[入口面 CLI のコマンド dispatch](02-architecture.md#入口面-cli-のコマンド-dispatch)を正本とする。

`~/.usagi/bin` が `PATH` に無い場合、installer は shell の設定ファイルへの追記方法を表示する。
導入後は診断画面で Git、Agent CLI、設定、daemon の状態を確認できる。

```bash
usagi doctor
```

## ソースからビルドする

`rust-toolchain.toml` の toolchain で release binary をビルドする。

```bash
git clone https://github.com/KKyosuke/usagi.git
cd usagi
cargo build --release
```

生成される binary は `target/release/usagi` にある。Cargo の bin directory へ導入する場合は次を使う。

```bash
cargo install --path . --locked
```

ソースからビルドした binary は、`USAGI_RUNTIME_MODE` を指定しなければ `~/.usagi/local/` を使う。
公開 release の artifact は `~/.usagi/` を使う。保存先と mode の決定規則は
[artifact の既定 mode](05-daemon.md#artifact-の既定-mode)を正本とする。

## 更新

最新版へ更新する場合は `usagi update`、公開済みの release を選ぶ場合は `-v` を使う。

```bash
usagi update
usagi update -v
```

更新後の CLI は次回起動から使われるため、起動中の TUI は終了して開き直す。
更新時の download・検証・atomic replacement と内部 daemon 同期は
[入口面 CLI のコマンド dispatch](02-architecture.md#入口面-cli-のコマンド-dispatch)、
live Agent を含む daemon の安全な引き継ぎと拒否条件は
[planned replacement](05-daemon.md#planned-replacement)を正本とする。

managed daemon 同期を持たない旧版から初めて更新する 1 回だけは、実行中の旧 `update` 自体を
遡及的に変更できないため binary の差し替えだけで終了する。その場合は更新後の `usagi update` または
`usagi daemon restart` をもう一度実行し、binary と daemon を揃える。

## shell 補完

`usagi completion <shell>` は CLI 定義から補完スクリプトを標準出力へ生成する。

```bash
# Bash: 現在の shell で読み込む
source <(usagi completion bash)

# Zsh: 補完関数の directory へ保存する
mkdir -p ~/.zfunc
usagi completion zsh > ~/.zfunc/_usagi

# Fish: user completion directory へ保存する
mkdir -p ~/.config/fish/completions
usagi completion fish > ~/.config/fish/completions/usagi.fish
```

Zsh で `~/.zfunc` をまだ使っていない場合は、次を `~/.zshrc` に追加する。
既に `compinit` を初期化している場合は、`fpath` の行だけをその初期化より前に置く。

```zsh
fpath=(~/.zfunc $fpath)
autoload -Uz compinit
compinit
```

生成元と補完候補の内部契約は
[入口面 CLI のコマンド dispatch](02-architecture.md#入口面-cli-のコマンド-dispatch)を正本とする。
