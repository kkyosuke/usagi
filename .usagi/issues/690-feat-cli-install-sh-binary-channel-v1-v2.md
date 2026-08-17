---
number: 690
title: feat(cli): install.sh に binary channel 名を導入して v1 / v2 を並置する
status: todo
priority: low
labels: [cli, build]
dependson: []
related: [691, 694]
created_at: 2026-08-17T22:49:02.982630+00:00
updated_at: 2026-08-17T23:22:06.402413+00:00
---

> **この issue は [#694](694-feat-cli-channel-switch-usagi-1-v1-v2.md) に置き換えられた。着手しないこと。**
> #694 が本 issue の内容を含み、任意 basename（`USAGI_BIN_NAME`）を **named channel** へ一般化する。
> 任意の名前を持ち込めると channel helper が管理対象を列挙できないため、名前の決め方を変えている。
> 背景は [document/proposals/17-v2-trial-channel.md](../../document/proposals/17-v2-trial-channel.md) の
> 「P2: channel switch」を参照する。

設計は [document/proposals/16-v1-v2-coexistence.md](../../document/proposals/16-v1-v2-coexistence.md) の
「設計 1: binary を channel 名で並置する」が正本。

## 背景

`scripts/install.sh` と `usagi update` はどちらも `${USAGI_HOME:-~/.usagi}/bin/usagi` へ rename する。
この path は runtime mode で分岐しないため、v1 と v2 で実体を 1 つしか置けない。これが v1 / v2 を
同じインストール先で共存させるうえで唯一の実質的な blocker である。

さらに現在の公開 release は v1 なので、ソースから起動した v2 で `usagi update` を実行すると
**v2 自身が v1 に置き換わる**。

## やること

### 1. `install.sh` に install 先 basename の入口を足す

- 環境変数 `USAGI_BIN_NAME`（既定 `usagi`）を読み、`TARGET="$BIN_DIR/$USAGI_BIN_NAME"` にする。
- 名前は `[A-Za-z0-9_-]+` に限る。path 区切り・`.`・空文字を含む値は `fail` で拒否し、`BIN_DIR` の
  外へ書かせない。
- `<base>/update.lock` は**共有のまま**にする。同じ directory への rename を直列化するのが目的で、
  channel ごとに分けると本来直列化したい組み合わせだけが漏れる。
- staged binary の `version` 出力と release version の一致検証は現行のまま使う。

### 2. `usagi update` が自分の channel 名を渡す

- `crates/cli/src/cli/commands/update.rs` は、自分の実行ファイルの basename を installer へ
  `USAGI_BIN_NAME` として渡す。これで v2 の update が v1 を上書きせず、v2 が v1 に化けない。
- **channel に対応する release が無ければ拒否する**。v2 の release channel が存在しない間、
  `usagi2` に対する update は v1 の archive を掴むため、誤って v1 binary を `usagi2` として
  置かないよう安全側に落とす。
- 実行ファイル path の解決は real IO なので合成ルートで束ね、usecase には basename を注入する。

### 3. digest 固定 test の更新

`update.rs` は `install.sh` を `include_bytes!` で同梱し、digest を unit test
（`embedded_installer_matches_its_immutable_digest_and_never_looks_up_main`）で固定している。
script 変更と digest 更新は**同じ変更**に含める。

### 4. docs

- `README.md` の「ソースから起動した v2 で `usagi update` を実行すると公開中の v1 バイナリが
  インストールされる」注記を、実装後の挙動へ更新する。
- `document/01-overview.md` の `usagi update` 行を実装後の挙動に合わせる。

## テスト

- `scripts/install.sh` の fixture test（`scripts/tests/` 配下に既存の script test と同じ形で）:
  既定名、明示 channel 名、不正名の拒否、lock の共有。
- `cargo test -p usagi-cli`: basename 引き渡し、release 不在時の拒否、digest 固定。
