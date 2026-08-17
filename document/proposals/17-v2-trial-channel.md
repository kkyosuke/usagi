# 17. v2 を試せる opt-in beta channel

> [設計提案一覧](README.md) ｜ [ドキュメント目次](../README.md) ｜ ← 前へ [v1 / v2 の共存](16-v1-v2-coexistence.md)

「新しい UI を試す」形で利用者が v2 を opt-in で試し、いつでも v1 へ戻せるようにするための設計である。
共存そのものの実態と制約は [16. v1 / v2 の共存](16-v1-v2-coexistence.md) が正本で、本書はその上に
**試用体験**（配布・切替・初期状態・戻り道）を載せる。

結論から述べる。**成立する。しかも試用に一番必要な「安全に戻れること」は既に無料で手に入っている**。
v2 の runtime mode は既定が `local` なので、v2 を試しても v1 の state を 1 byte も触らない
（[16 の実態表](16-v1-v2-coexistence.md#領域ごとの共有分離の実態)）。試用の可逆性は設計済みであり、
足りないのは**配布物・切替手段・初期状態・告知**の 4 点である。

## 目次

- [既に揃っているもの](#既に揃っているもの)
- [足りない 4 点](#足りない-4-点)
- [P1: v2 prerelease channel](#p1-v2-prerelease-channel)
- [P2: channel switch](#p2-channel-switch)
- [P3: 新しい UI を空にしない](#p3-新しい-ui-を空にしない)
- [P4: 戻り道と告知](#p4-戻り道と告知)
- [試用中に共有されるもの・されないもの](#試用中に共有されるものされないもの)
- [却下した代替案](#却下した代替案)
- [既知の後続作業](#既知の後続作業)
- [実装 issue](#実装-issue)

## 既に揃っているもの

試用 channel を作るうえで、**変更しなくてよい**ことを先に確定させる。

| 前提 | 現状 | 意味 |
|---|---|---|
| 試用が v1 を壊さない | v2 の runtime mode は既定 `local`。global は `<base>/local/`、project は `<repo>/.usagi/local/` | 試用は本質的に可逆。戻すときデータ移行が要らない |
| stable 利用者が誤って beta へ行かない | `install.sh` の既定経路は `/releases/latest`（GitHub が prerelease を除外）＋ `^v?\d+\.\d+\.\d+$` の厳格 filter | 二重に prerelease を弾く。opt-in は構造的に保証される |
| prerelease tag を指定して入れられる | `USAGI_VERSION` の検証 glob `v[0-9]*.[0-9]*.[0-9]*` は `v3.0.0-beta.1` を通す | install script 側の version 検証は変更不要 |
| v2 の binary が installer の検証を通る | `verify_archive` は archive 内の唯一の entry 名が `usagi` であることを要求し、root package も `usagi` を出す。`read_version` は `usagi <version>` 形式を要求し、v2 の `version` command はその形式で出す | archive 形式・version 検証は変更不要 |
| issue / memory が試用中も見える | `<repo>/.usagi/issues/` / `memory/` / 採番 authority は v1 と共有（[16](16-v1-v2-coexistence.md#領域ごとの共有分離の実態)） | 試用でも自分のタスクが並ぶ |

## 足りない 4 点

```
P1 配布物     v2 の release artifact が存在しない（release.yml は v1/Cargo.toml 固定）
P2 切替       usagi という 1 つの名前でどちらを起動するか選べない
P3 初期状態   v2 は local mode で空から始まるので「新しい UI」が空に見える
P4 戻り道     戻す手段と、戻したときに何が見えなくなるかの告知
```

## P1: v2 prerelease channel

`release.yml` は reusable（`workflow_call`）だが、build 対象が `--manifest-path v1/Cargo.toml` に
固定され、**prerelease を作る手段が無い**。ここに 2 つの input を足す。

| input | 用途 |
|---|---|
| `manifest` | build 対象の Cargo manifest（既定 `v1/Cargo.toml`。v2 は root の `Cargo.toml`） |
| `prerelease` | `softprops/action-gh-release` の `prerelease` へ渡す（既定 `false`） |

### `prerelease: true` は必須である

これが本設計で最も事故に近い一点である。`release.yml` は現在 `prerelease` を渡していないため、
v2 の release を作るとそれが `/releases/latest` になる。すると `install.sh` の**既定経路が
stable 利用者を v2 へ引き込む**。opt-in が opt-out に反転し、`resolve_latest_release` の
厳格 filter（`^v?\d+\.\d+\.\d+$`）も `v3.0.0` 形式の tag なら通してしまう。
beta channel の release は必ず prerelease として公開する。

### tag は `v3.0.0-beta.N` にする

v1 の version は 2.9.1、root（v2）の version は 2.6.0 で、**semver では v2 のほうが小さい**。
「v2」はコードベースの世代であって semver major ではない。`2.x` の続きとして tag を切ると、
install script の version 比較、release notes の `git tag --sort=-v:refname`、利用者から見た
新旧の順序がすべて逆転する。v2 は**次の major を主張する**。

- beta: `v3.0.0-beta.1`, `v3.0.0-beta.2`, …
- 正式版: `v3.0.0`（cutover 時。[16 の前提](16-v1-v2-coexistence.md#前提と制約)どおり v1 は退役する）

release notes の `PREV_TAG` 解決（`git tag --sort=-v:refname` から自分以外の先頭）は、beta tag を
挟むと v1 系の履歴を丸ごと拾う。**同じ channel 内の直前 tag**に絞る。

### beta の起点は自動化しない

`auto-release.yml` は `v1/Cargo.toml` の version 変更を監視して自動でタグを切る。v2 beta は
これと対称に root `Cargo.toml` を監視できるが、**beta 中は手動 dispatch にする**。root の version は
`build.rs` の identity や `usagi version` の表示にも使われる開発中の値で、bump のたびに公開 release が
出るのは beta の運用として重い。

### `-v` の release picker

`select_release` は `/releases` から `^v?\d+\.\d+\.\d+$` だけを拾うので、beta tag は選択肢に出ない。
beta channel を選んでいるときだけ prerelease を候補に含める（stable の picker は現状のまま）。

## P2: channel switch

`usagi` という 1 つの名前で、選んだ channel が起動する状態を作る。

```
~/.usagi/bin/usagi-v1        v1 の実体（stable channel）
~/.usagi/bin/usagi-v2        v2 の実体（beta channel）
~/.usagi/bin/usagi        →  symlink: 現在アクティブな実体
~/.usagi/bin/usagi-channel   切替 helper（installer が同梱する）
```

| 決定 | 理由 |
|---|---|
| **symlink 自体を状態にする**（別の pref file を持たない） | pref file と実際に起動する binary が desync しうる。`readlink` すれば現在の channel が一意に判る |
| 切替は**小さな helper script**（compiled shim にしない） | shim は毎回の起動に exec を挟む。Unix は `execvp` で置き換えられるが Windows に exec が無く、spawn + wait + signal / exit code 転送を自作することになる。PTY 中心の usagi でそこを自作する risk は、symlink 差し替え 1 回のコストに見合わない。symlink 方式は常時コストがゼロである |
| 既存 install からの移行は rename + symlink | 現在の `bin/usagi` は実ファイル。初回だけ `usagi-v1` へ rename して symlink を張る。同一 filesystem なので rename は atomic |
| `update.lock` は共有のまま | 同じ directory への rename を直列化するのが目的。channel ごとに分けると、まさに直列化したい組み合わせだけが漏れる |

helper の interface は最小に保つ。

| command | 動作 |
|---|---|
| `usagi-channel status` | 現在の channel と、各 channel の install 済み version を表示する |
| `usagi-channel use <stable\|beta>` | symlink を差し替える。対象が未 install なら**何も変更せず**、install 方法を案内して失敗する |

[#690](../../.usagi/issues/690-feat-cli-install-sh-binary-channel-v1-v2.md) の `USAGI_BIN_NAME` が
この下敷きになる。#690 を「任意の basename」から「**named channel**」へ一般化し、install 先の名前を
channel 定義から導く（利用者が任意の名前を持ち込めると、helper が管理対象を列挙できない）。

## P3: 新しい UI を空にしない

v2 は `local` mode なので、`<base>/local/workspaces.json` は存在せず **workspace 一覧が空**で始まる。
「新しい UI を試す」で空の画面が出るのは壊れて見える。v2 の初回起動で v1 の workspace 一覧を seed する。

| 性質 | 内容 |
|---|---|
| 方向 | 一方向。v1 の `<base>/workspaces.json` を**読むだけ**で、書かない・移動しない |
| 回数 | 1 回だけ。marker を置いて再実行しない。再実行すると v2 で削除した workspace が復活する |
| 失敗時 | seed に失敗しても起動を止めない。空の一覧で開き、error log に残す |
| 対象 | workspace の登録と最終利用日時のみ。settings は seed しない（v1 と v2 で schema と項目が異なるため、誤った値を引き継ぐより既定から始めるほうが安全） |

issue / memory は既に共有なので seed 不要である（[16](16-v1-v2-coexistence.md#領域ごとの共有分離の実態)）。

## P4: 戻り道と告知

戻すのは `usagi-channel use stable` だけで、**データ移行は要らない**。v2 の state は `<base>/local/` と
`<repo>/.usagi/local/` に残るので、また beta に戻せば続きから使える。

ただし戻したときに見えなくなるものがある。これは試用の性質上避けられないので、**隠さずに告知する**。

| 試用中に v2 で作ったもの | v1 へ戻したときの見え方 |
|---|---|
| issue / memory | **そのまま見える**（共有） |
| workspace 登録 | v1 の一覧には出ない（v2 の登録は `local/` にある）。v1 側で改めて開けばよい |
| session worktree と `usagi/<name>` branch | **v1 の一覧に出ない**。git 上には実体として残る |

session が引き継がれないのは lifecycle state が別だからで、[16](16-v1-v2-coexistence.md#衝突しないと確認した点)
のとおり v2 は自分の state 外の worktree を掃除しないため**壊れはしない**が、v1 から見ると
一覧に出ない worktree が残る。`usagi-channel use stable` は、beta 側に live session がある場合に
その数を示して確認を求める。

告知は installer の完了メッセージと `README.md` で行い、**v1 には手を入れない**
（[16 の前提](16-v1-v2-coexistence.md#前提と制約)）。

## 試用中に共有されるもの・されないもの

利用者に見せるべき要約は次の 1 表である。

```
共有される     issue / memory / issue 番号        → 試用中も自分のタスクが並ぶ
分離される     workspace 登録 / 設定 / session   → v1 の状態は壊れない。戻すのは安全
残る制約       同じ workspace を v1 と v2 で同時に開かない
```

最後の 1 行は機械的に防げない。v2 は workspace fence で自分同士を排他するが、v1 はこの fence を
取らない（[16](16-v1-v2-coexistence.md#衝突しないと確認した点)）。**channel を切り替えて使う**運用に
していれば同時起動は自然に起きないため、試用体験としてはこの制約が表に出にくい。それでも
両方を同時に起動できる状態は残るので、告知に含める。

## 却下した代替案

| 代替案 | 却下理由 |
|---|---|
| v1 の Welcome / Config に「新しい UI を試す」行を足す | 導線としては最良だが、v1 は出荷物で coverage 100% gate 対象である（[16 の前提](16-v1-v2-coexistence.md#前提と制約)）。試用の導線のために shipped code へ変更 risk を入れない |
| compiled shim が `usagi` を受けて channel へ exec する | 毎回の起動経路に入る。Windows に exec が無く signal / exit code / stdio の転送を自作することになり、PTY 中心の usagi では失敗モードが増える。symlink 差し替えなら常時コストがゼロ |
| 試用を production mode で走らせる | v1 と同じ path を掴むので可逆性を失う。試用の最大の価値（安全に戻れること）を捨てることになる |
| v2 beta を専用 runtime mode（`beta` など）に置く | mode は 3 つで足りており、`local` が既にこの用途である。mode を増やすと child への転送・fence・data home の対応表がすべて広がる |
| v2 を `2.x` の続きとして tag する | v1 が 2.9.1、root が 2.6.0 なので semver 順序が逆転する。version 比較・release notes・利用者の認識がすべて壊れる |
| `usagi update` に beta channel を判定させて自動で beta へ上げる | 利用者が明示的に選んでいない channel 変更になる。opt-in を壊す |

## 既知の後続作業

試用の state は `<base>/local/` と `<repo>/.usagi/local/` にある。v2 が正式版になるとき
（`v3.0.0`）は production mode へ移り、この `local/` から base への移行が必要になる。cutover の
一部であり本設計には含めない。[16 の設計 3](16-v1-v2-coexistence.md#設計-3-production-mode-の重なりを-doctor-で可視化する)
の doctor 警告が、その移行前に production と v1 が重なっている状態を可視化する。

## 実装 issue

| # | 対応 | 内容 |
|---|---|---|
| [#693](../../.usagi/issues/693-build-release-yml-manifest-prerelease-input-v2-beta-channel.md) | P1 | `release.yml` に `manifest` / `prerelease` input、v2 beta の tag 規約と release notes の channel 内 PREV_TAG |
| [#694](../../.usagi/issues/694-feat-cli-channel-switch-usagi-1-v1-v2.md) | P2 | named channel での並置、`usagi-channel` helper、symlink 移行 |
| [#695](../../.usagi/issues/695-feat-core-v2-v1-workspace-read-only-seed.md) | P3 | v2 初回起動時の v1 workspace 一覧の read-only seed |
| [#696](../../.usagi/issues/696-docs-v2-channel.md) | P4 | 戻り道の告知、live session がある場合の確認、README と installer メッセージ |

依存順は P1 →（P2, P3 は並行）→ P4。P1 が無いと試す対象が存在しないため、これが起点である。
P2 は [#690](../../.usagi/issues/690-feat-cli-install-sh-binary-channel-v1-v2.md) を named channel へ
一般化する形で置き換える。
