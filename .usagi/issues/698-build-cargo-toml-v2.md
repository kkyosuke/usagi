---
number: 698
title: build: リリース起点をルート Cargo.toml へ切り替え、v2 を出荷・インストール可能にする
status: done
priority: high
labels: [build, ci, docs]
dependson: []
related: [697]
created_at: 2026-08-18T00:29:13.882960+00:00
updated_at: 2026-08-18T00:29:13.882960+00:00
---

## 概要

リリース経路が `v1/Cargo.toml` の version 変更を起点に **v1 を出荷**する構成だったため、v2 を公開
release として配れなかった。起点をルート `Cargo.toml` へ切り替え、v2 を出荷・インストールできるようにする。

[#697](697-fix-core-artifact-runtime-mode-launchd-plist-mode.md) の `production` feature に依存する。
この feature 無しで出荷すると利用者のデータが `~/.usagi/local/` に入る。

## 対象 platform は 3 つ

v1 は 4 プラットフォーム（Linux / macOS amd64・arm64 / Windows）を build していたが、v2 は
**Linux amd64 / macOS amd64 / macOS arm64 の 3 つ**にする。

- `scripts/install.sh` の `platform_asset` は darwin / linux だけを受け付け、それ以外は `fail` する。
  つまり installer は元々 Windows の archive を配れていない。
- v2 は Unix domain socket の IPC（`crates/daemon/src/infrastructure/unix_transport.rs`）と Unix 専用の
  process / permission API に依存するため、Windows ではコンパイルできない。実測（`cargo check --target
  x86_64-pc-windows-msvc -p usagi`）で `usagi-core` だけで 8 件のエラーが出る:
  `libc::kill` / `libc::SIGKILL` / `libc::pid_t` / `std::os::unix` / `PermissionsExt::mode` /
  `CommandExt::process_group` が解決しない。

## やったこと

### workflow

| ファイル | 変更 |
|---|---|
| `release.yml` | build 対象をルート `Cargo.toml` へ。`--features production` を付ける。target 行列から Windows を削除し、archive 生成の zip 分岐も削除（installer が読む tar.gz のみ）。toolchain は `rust-toolchain.toml` の pin を使い、cross target は `rustup target add` で pin した toolchain へ入れる |
| `auto-release.yml` | 監視対象を `v1/Cargo.toml` → ルート `Cargo.toml` |
| `create-release-pr.yml` | 書き換え対象をルート `Cargo.toml` へ。lock 更新は `cargo check --workspace` |
| `release-build-check.yml` | ルート `Cargo.toml` / `Cargo.lock` に加え、**リリース経路の workflow 自身と `rust-toolchain.toml`** も trigger に含める。3 target を `--features production` で release build し、host target では installer の version 出力契約（`usagi <version>`）も検証する |

`release-build-check.yml` の trigger に workflow 自身を含めたのは、リリース経路を変更する PR では version が
動かないため、version だけを trigger にすると経路の変更が無検証でマージされてしまうからである。

### toolchain の注意

`dtolnay/rust-toolchain` の `targets:` は**日付なし nightly**へ target を入れるため、`rust-toolchain.toml` が
pin した toolchain には届かない（component と同じ drift。正本は
[6. 開発規約](../../document/06-conventions.md#品質チェックリスク比例の-gate)）。`rustup target add` を
別 step にして pin した toolchain へ入れる。

### install.sh は変更不要

実際に v2 の release artifact を作って契約を確認した。

| installer の要求 | v2 の実態 |
|---|---|
| `platform_asset`: darwin / linux のみ | 対象 3 platform と一致 |
| `verify_archive`: archive 内の唯一の top-level entry が `usagi` かつ正規ファイル | ルートパッケージの bin 名が `usagi` なので一致 |
| `read_version`: `usagi <version>` 形式 | `version` command がこの形式で出す |
| `.sha256` / `.version` artifact 必須 | `release.yml` が従来どおり生成 |

### docs

- `document/06-conventions.md` のリリース節を全面更新し、**出荷 artifact の要件**（feature / toolchain /
  platform / archive / verification artifact / version 出力）を表にした。CI 表の
  `release-build-check.yml` / `v1-test.yml` / `v1-coverage.yml` / `tui-e2e.yml` の記述も実態に合わせた。
- `document/01-overview.md` の「v1 との関係」を、出荷対象が v2 であることへ更新した。
- `README.md` に installer による導入手順を追加し、「現在の公開 release は v1」注記を削除した。
  ソースビルドが `~/.usagi/local/` を使うことも明記した。

## version は別 PR で上げる

**この PR は version を上げない。** 上げると merge がそのままリリースを発火させてしまうため、
規約どおり「リリースしたい変更をマージ → version を上げる PR をマージ」の 2 段にする。

v2 として最初に出す version は既存の v1 release（`2.9.1`）より大きくする必要がある。小さいと
`/releases/latest` が v1 のままになり、installer の既定経路が新しい v2 を選ばない。ルートは現在 `2.6.0`
なので、**`3.0.0`** を推奨する（rewrite でデータ配置も変わる別実装であり、major bump が利用者に正しく伝わる）。
