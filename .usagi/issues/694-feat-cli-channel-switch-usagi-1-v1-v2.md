---
number: 694
title: feat(cli): channel switch を入れて usagi 1 つの名前で v1 / v2 を切り替える
status: todo
priority: high
labels: [cli, build]
dependson: [693]
related: [690]
created_at: 2026-08-17T23:20:22.688059+00:00
updated_at: 2026-08-17T23:20:22.688059+00:00
---

設計は [document/proposals/17-v2-trial-channel.md](../../document/proposals/17-v2-trial-channel.md) の
「P2: channel switch」が正本。

**この issue は [#690](../../.usagi/issues/690-feat-cli-install-sh-binary-channel-v1-v2.md) を置き換える**。
#690 の「任意の basename を指定できる `USAGI_BIN_NAME`」を、named channel へ一般化する
（利用者が任意の名前を持ち込めると helper が管理対象を列挙できない）。

## 目標のレイアウト

```
~/.usagi/bin/usagi-v1        v1 の実体（stable channel）
~/.usagi/bin/usagi-v2        v2 の実体（beta channel）
~/.usagi/bin/usagi        →  symlink: 現在アクティブな実体
~/.usagi/bin/usagi-channel   切替 helper（installer が同梱する）
```

## やること

### 1. install.sh を named channel 化する

- `USAGI_CHANNEL`（`stable` | `beta`、既定 `stable`）を読み、install 先の basename を channel 定義から導く。
  利用者が任意 basename を渡す入口は作らない。
- `stable` は v1 の stable release、`beta` は v2 の prerelease（`v3.0.0-beta.N`）を対象にする。
- `select_release`（`usagi update -v` の picker）は `^v?\d+\.\d+\.\d+$` だけを拾うので beta tag が候補に
  出ない。**beta channel を選んでいるときだけ prerelease を候補に含める**（stable の picker は現状のまま）。
- `USAGI_VERSION` の検証 glob `v[0-9]*.[0-9]*.[0-9]*` は `v3.0.0-beta.1` を通すので変更不要。
  `verify_archive`（archive 内の唯一の entry 名が `usagi`）と `read_version`（`usagi <version>` 形式）も
  v2 が満たすので変更不要。
- `<base>/update.lock` は**共有のまま**。同じ directory への rename を直列化するのが目的で、channel ごとに
  分けるとまさに直列化したい組み合わせだけが漏れる。

### 2. 既存 install からの移行

現在の `bin/usagi` は実ファイルである。初回だけ `usagi-v1` へ rename して symlink を張る。
同一 filesystem 上の rename なので atomic。`bin/usagi` が既に symlink なら移行済みとして何もしない。

### 3. `usagi-channel` helper

**symlink 自体を状態にする。別の pref file を持たない**（pref file と実際に起動する binary が
desync しうる。`readlink` すれば現在の channel が一意に判る）。

| command | 動作 |
|---|---|
| `usagi-channel status` | 現在の channel と、各 channel の install 済み version を表示する |
| `usagi-channel use <stable\|beta>` | symlink を差し替える。対象が未 install なら**何も変更せず** install 方法を案内して失敗する |

- compiled shim にはしない。shim は毎回の起動経路に入り、Windows に exec が無いため
  spawn + wait + signal / exit code / stdio の転送を自作することになる。symlink 差し替えは 1 回で済み
  常時コストがゼロである。
- symlink の差し替えは temp symlink + rename で atomic に行う（差し替え中に `usagi` が存在しない窓を作らない）。

### 4. `usagi update` が自分の channel へ入れる

`crates/cli/src/cli/commands/update.rs` は自分の実行ファイルの basename から channel を判定し、
installer へ渡す。これで v2 の update が v1 を上書きせず、v2 が v1 に化けない。
**channel に対応する release が無ければ拒否する**（誤って v1 binary を beta channel に置かない）。

`update.rs` は `install.sh` を `include_bytes!` で同梱し digest を unit test で固定しているので、
script 変更と digest 更新は同じ変更に含める。

## テスト

- `scripts/tests/` の script fixture test: 既定 channel、beta channel、未 install channel への `use`、
  実ファイルからの symlink 移行、symlink 差し替えの atomicity、lock の共有。
- `cargo test -p usagi-cli`: channel 判定、release 不在時の拒否、digest 固定。
