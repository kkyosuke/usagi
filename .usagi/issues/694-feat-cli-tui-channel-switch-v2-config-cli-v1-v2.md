---
number: 694
title: feat(cli,tui): channel switch を v2 の Config と CLI に持たせて v1 / v2 を切り替える
status: todo
priority: high
labels: [cli, build]
dependson: [693]
related: [690, 697]
created_at: 2026-08-17T23:20:22.688059+00:00
updated_at: 2026-08-17T23:33:17.914407+00:00
---

設計は [document/proposals/17-v2-trial-channel.md](../../document/proposals/17-v2-trial-channel.md) の
「P2: channel switch」が正本。

**この issue は [#690](690-feat-cli-install-sh-binary-channel-v1-v2.md) を置き換える**。
#690 の「任意の basename を指定できる `USAGI_BIN_NAME`」を named channel へ一般化する
（任意の名前を持ち込めると channel を列挙できない）。

## 目標のレイアウト

```
~/.usagi/bin/usagi-v1        v1 の実体（stable channel）
~/.usagi/bin/usagi-v2        v2 の実体（beta channel）
~/.usagi/bin/usagi        →  symlink: 現在アクティブな実体
```

切替は **v2 自身が持つ**。専用の helper script も compiled shim も置かない。

| 面 | 経路 | 使う場面 |
|---|---|---|
| v2 の Global Config | `Version  [ v2 (beta) ]` の行で v1 / v2 を選び、Save で symlink を差し替える | 試用をやめて v1 へ戻る（**主経路**） |
| v2 の CLI | `usagi channel status` / `usagi channel use <stable\|beta>` | v1 に居る状態から beta へ戻る（`usagi-v2 channel use beta` と実体名で呼ぶ） |
| installer | `USAGI_CHANNEL=beta` | 最初に試し始める |

**戻りたくなるのは v2 を使っている最中なので、戻り道は v2 の UI の中にあるべきである。**
opt-in は installer という明示的な行為で始まるので、v1 側の導線は要らない（v1 は出荷物なので置けない）。

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
- **既存 install からの移行**: 現在の `bin/usagi` は実ファイル。初回だけ `usagi-v1` へ rename して
  symlink を張る。同一 filesystem なので rename は atomic。既に symlink なら移行済みとして何もしない。
- `<base>/update.lock` は**共有のまま**。同じ directory への rename を直列化するのが目的で、channel ごとに
  分けるとまさに直列化したい組み合わせだけが漏れる。

### 2. `usagi channel` CLI

| command | 動作 |
|---|---|
| `usagi channel status` | 現在の channel と、各 channel の install 済み version を表示する |
| `usagi channel use <stable\|beta>` | symlink を差し替える。対象が未 install なら**何も変更せず**、install 方法を案内して失敗する |

### 3. Global Config の `Version` 行

- 既存の Config 画面の項目として追加する（`↑↓` で選び `←→` で変更、dirty のときだけ Save）。
- **Global Config にだけ置く**。channel は machine 全体の状態で、workspace 単位の設定ではない。
- 行が**選択不可**になる条件と、その理由の表示:
  - symlink 経由で起動していない（source build・`cargo run`・実体直叩き）。存在しない symlink を作らない。
  - 対象 channel が未 install。存在しない実体を指す symlink を張らない。
- v1 を選んだときの live runtime の扱いは [#696](696-feat-tui-docs-v1-live-runtime-admission.md) が持つ。

### 4. symlink の扱い（共通）

- **symlink 自体を状態にする。`settings.json` に channel を書かない。** 実際に起動する binary を決めるのは
  symlink なので、設定に持つと実態と desync する。現在の channel は `readlink` で一意に決まる。
- 差し替えは **temp symlink + rename** で atomic に行う（`usagi` が存在しない窓を作らない）。
- 走っている v2 が自分を指す symlink を差し替えてよい。Unix では実行中の process が inode を掴んでいるため、
  張り替えは現に走っている v2 に影響しない。
- **適用は次の起動から**。`usagi update` が既に同じ契約（反映には再起動が必要）を持つので、それに揃える。
  走っている v2 を exec で v1 に置き換えることはしない（理由は proposal 17 の却下表）。

### 5. `usagi update` が自分の channel へ入れる

`crates/cli/src/cli/commands/update.rs` は自分の実行ファイルの basename から channel を判定し、
installer へ渡す。**channel に対応する release が無ければ拒否する**（誤って v1 binary を beta channel に
置かない）。`install.sh` は `include_bytes!` で同梱され digest を unit test で固定しているので、
script 変更と digest 更新は同じ変更に含める。

## テスト

- `scripts/tests/` の script fixture test: 既定 channel、beta channel、未 install channel への `use`、
  実ファイルからの symlink 移行、symlink 差し替えの atomicity、lock の共有。
- `cargo test -p usagi-cli`: `channel status` / `use` の出力と拒否、channel 判定、release 不在時の拒否、digest 固定。
- `cargo test -p usagi-tui`: Config の `Version` 行の選択・dirty・Save、選択不可の 2 条件とその表示。
