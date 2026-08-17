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
- [release build の既定 mode](#release-build-の既定-mode)
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
```

切替は **v2 自身が持つ**。専用の helper script も compiled shim も置かない。

| 面 | 経路 | 使う場面 |
|---|---|---|
| v2 の Global Config | `Version  [ v2 (beta) ]` の行で v1 / v2 を選び、Save で symlink を差し替える | 試用をやめて v1 へ戻る（**主経路**） |
| v2 の CLI | `usagi channel status` / `usagi channel use <stable\|beta>` | v1 に居る状態から beta へ戻る（`usagi-v2 channel use beta` と実体名で呼ぶ） |
| installer | `USAGI_CHANNEL=beta` で beta を install し、symlink を beta へ向ける | 最初に試し始める |

**戻りたくなるのは v2 を使っている最中なので、戻り道は v2 の UI の中にあるべきである。**
v1 は出荷物なので v1 側に導線は置けないが、**opt-in は installer という明示的な行為で始まる**ので
v1 側の導線は要らない。UI に必要なのは戻り道だけである。

symlink の扱いは次のとおり。

| 決定 | 理由 |
|---|---|
| **symlink 自体を状態にする**（`settings.json` に channel を書かない） | 実際に起動する binary を決めるのは symlink なので、設定に持つと実態と desync する。`readlink` すれば現在の channel が一意に決まる |
| 差し替えは temp symlink + rename | `usagi` が存在しない窓を作らない |
| 走っている v2 が自分を指す symlink を差し替えてよい | Unix では実行中の process が inode を掴んでいるため、symlink の張り替えは現に走っている v2 に影響しない |
| Config の行は **Global Config にだけ**置く | channel は machine 全体の状態で、workspace 単位の設定ではない |
| symlink 経由で起動していないときは行を選択不可にする | source build・`cargo run`・実体直叩きでは管理対象の symlink が無い。存在しない symlink を作らない |
| 対象 channel が未 install なら選択不可にする | 存在しない実体を指す symlink を張らない。install 方法を案内する |
| 既存 install からの移行は rename + symlink | 現在の `bin/usagi` は実ファイル。初回だけ `usagi-v1` へ rename して symlink を張る。同一 filesystem なので rename は atomic |
| `update.lock` は共有のまま | 同じ directory への rename を直列化するのが目的。channel ごとに分けると、まさに直列化したい組み合わせだけが漏れる |

適用は**次の起動から**である。`usagi update` が既に同じ契約（反映には再起動が必要）を持つので、それに揃える。
走っている v2 を exec で v1 に置き換えることはしない（[却下した代替案](#却下した代替案)）。

[#690](../../.usagi/issues/690-feat-cli-install-sh-binary-channel-v1-v2.md) の `USAGI_BIN_NAME` が
install 側の下敷きになる。#690 を「任意の basename」から「**named channel**」へ一般化し、install 先の
名前を channel 定義から導く（利用者が任意の名前を持ち込めると、channel を列挙できない）。

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

戻り道は [P2](#p2-channel-switch) の Global Config の `Version` 行で、**データ移行は要らない**。v2 の state は
`<base>/local/` と `<repo>/.usagi/local/` に残るので、また beta へ戻せば続きから使える。

Config で v1 を選んだときに解決しなければならないのは **live runtime** である。symlink を戻しても v2 の
daemon は動き続け、PTY と workspace fence を持ったままになる。ここは新しい規則を作らず、**既存の
`daemon stop` の admission をそのまま使う**（[5. daemon](../05-daemon.md#daemon-process-lifecycle)）。

| beta 側の状態 | Config で v1 を選んだときの動作 |
|---|---|
| daemon が停止している | symlink を差し替え、次の起動から v1 になることを伝える |
| daemon は動いているが live Agent / terminal が無い | daemon を停止してから差し替える |
| live Agent / terminal がある | **差し替えない**。件数を示して pane を閉じるか明示的に手放すことを促す（`daemon stop` が `--force` を要求するのと同じ判断） |

そのうえで、戻したときに見えなくなるものを告知する。これは試用の性質上避けられないので、**隠さない**。

| 試用中に v2 で作ったもの | v1 へ戻したときの見え方 |
|---|---|
| issue / memory | **そのまま見える**（共有） |
| workspace 登録 | v1 の一覧には出ない（v2 の登録は `local/` にある）。v1 側で改めて開けばよい |
| session worktree と `usagi/<name>` branch | **v1 の一覧に出ない**。git 上には実体として残る |

session が引き継がれないのは lifecycle state が別だからで、[16](16-v1-v2-coexistence.md#衝突しないと確認した点)
のとおり v2 は自分の state 外の worktree を掃除しないため**壊れはしない**が、v1 から見ると
一覧に出ない worktree が残る。この告知は Config で v1 を選んだ時点で出す。

告知は Config の確認、installer の完了メッセージ、`README.md` で行い、**v1 には手を入れない**
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

## release build の既定 mode

`runtime_mode()` は `USAGI_RUNTIME_MODE` が無ければ **debug / release build とも `local`** を選び、
production は明示指定を要求する（[5. daemon](../05-daemon.md#daemon-data-directory)）。この非対称は
意図的で、**危険な向き（実データを触る）に明示的な行為を要求する** fail-safe である。v2 は v1 が本番で
使われている同じマシン・同じ repository の中で開発されるため、`cargo run` や test が実 state を掴めては
ならない。

この既定は**試用にとっては正しい**。beta artifact が `local` を選ぶことが、そのまま
「試用が v1 を壊さない」根拠になる（[既に揃っているもの](#既に揃っているもの)）。

一方で**正式版にとっては正しくない**。`install.sh` も release artifact も `USAGI_RUNTIME_MODE` を
設定しないため、**今のまま v2 を出荷すると利用者のデータが `~/.usagi/local/` に入る**。`local` は
開発時の概念であって、利用者に見せる置き場所ではない。

env では解決しない。[#542](../../.usagi/issues/542-fix-daemon-fence-workspace-mode-home.md) が記録している
とおり利用者自身の shell を統一する強制力は無く、env に依存した既定は「plain shell で起動したら別の世界に
入る」を残す。**artifact に既定を焼き込む**のが答えである。

| artifact | 既定 mode | 根拠 |
|---|---|---|
| source build（`cargo run` / `cargo test`） | `local` | 開発中に実 state を触らせない |
| beta channel artifact | `local` | 試用が可逆であること |
| stable artifact（`v3.0.0`） | `production` | 利用者のデータを `~/.usagi` 直下に置く |

`USAGI_RUNTIME_MODE` は引き続きすべてを上書きできる（開発・調査の経路を塞がない）。

### launchd に mode が伝わらない

`daemon install-service` が書く plist は**環境変数を持たない**設計である
（[5. daemon](../05-daemon.md#launchd-supervision)）。したがって supervise される `daemon serve` は
env 不在から `local` を選ぶ。一方 plist の stderr log path は install した process の
**mode-selected** data directory から作る。この 2 つが食い違うため、production mode から
install-service すると **log は production 配下、daemon は local** という組み合わせになる。

artifact に既定を焼き込めば既定同士は一致するが、`production` 既定の stable artifact では
**plist に mode を明記する**必要がある（plist が env を持たない現在の設計を、mode だけは例外にするか、
`daemon serve` に mode を渡す引数を足すかの判断を伴う）。

## 却下した代替案

| 代替案 | 却下理由 |
|---|---|
| v1 の Welcome / Config に「新しい UI を試す」行を足す | 導線としては最良だが、v1 は出荷物で coverage 100% gate 対象である（[16 の前提](16-v1-v2-coexistence.md#前提と制約)）。試用の導線のために shipped code へ変更 risk を入れない |
| compiled shim が `usagi` を受けて channel へ exec する | 毎回の起動経路に入る。Windows に exec が無く signal / exit code / stdio の転送を自作することになり、PTY 中心の usagi では失敗モードが増える。symlink 差し替えなら常時コストがゼロ |
| 走っている v2 が exec で v1 に置き換わり「即座に切替」を実現する | v2 は daemon と PTY を持つ。live な pane を v1 へ引き継ぐ IPC は存在しないので、即座の切替は結局 live runtime の破棄を伴う。`usagi update` と同じ「次の起動から」に揃えるほうが予測可能である |
| channel を `settings.json` に持つ | 実際に起動する binary を決めるのは symlink なので、設定と実態が desync する。symlink を単一の情報源にする |
| 専用の `usagi-channel` helper script を installer が同梱する | 当初案。v2 の CLI に `channel` を置けば足り、install する component を増やさずに済む。さらに戻り道は v2 の Config に置くほうが発見しやすいため差し替えた |
| release でも env（`USAGI_RUNTIME_MODE=production`）で mode を選ばせる | 利用者の shell を統一する強制力が無い（[#542](../../.usagi/issues/542-fix-daemon-fence-workspace-mode-home.md)）。plain shell 起動が別の世界に入る余地を残す |
| 試用を production mode で走らせる | v1 と同じ path を掴むので可逆性を失う。試用の最大の価値（安全に戻れること）を捨てることになる |
| v2 beta を専用 runtime mode（`beta` など）に置く | mode は 3 つで足りており、`local` が既にこの用途である。mode を増やすと child への転送・fence・data home の対応表がすべて広がる |
| v2 を `2.x` の続きとして tag する | v1 が 2.9.1、root が 2.6.0 なので semver 順序が逆転する。version 比較・release notes・利用者の認識がすべて壊れる |
| `usagi update` に beta channel を判定させて自動で beta へ上げる | 利用者が明示的に選んでいない channel 変更になる。opt-in を壊す |

## 既知の後続作業

試用の state は `<base>/local/` と `<repo>/.usagi/local/` にある。stable artifact が `production` を
既定にする（[release build の既定 mode](#release-build-の既定-mode)）ため、beta で試した利用者が
`v3.0.0` へ上がるときに **`local/` から base への一方向 migration** が必要になる。cutover の一部であり
本設計には含めないが、artifact 既定を決める時点でこの migration の存在を前提にする。
[16 の設計 3](16-v1-v2-coexistence.md#設計-3-production-mode-の重なりを-doctor-で可視化する) の doctor 警告が、
その移行前に production と v1 が重なっている状態を可視化する。

## 実装 issue

| # | 対応 | 内容 |
|---|---|---|
| [#693](../../.usagi/issues/693-build-release-yml-manifest-prerelease-input-v2-beta-channel.md) | P1 | `release.yml` に `manifest` / `prerelease` input、v2 beta の tag 規約と release notes の channel 内 PREV_TAG |
| [#694](../../.usagi/issues/694-feat-cli-tui-channel-switch-v2-config-cli-v1-v2.md) | P2 | named channel での並置、`usagi channel` CLI、Global Config の `Version` 行、symlink 移行 |
| [#695](../../.usagi/issues/695-feat-core-v2-v1-workspace-read-only-seed.md) | P3 | v2 初回起動時の v1 workspace 一覧の read-only seed |
| [#696](../../.usagi/issues/696-feat-tui-docs-v1-live-runtime-admission.md) | P4 | 戻り道の live runtime admission と告知、README と installer メッセージ |
| [#697](../../.usagi/issues/697-fix-core-artifact-runtime-mode-launchd-plist-mode.md) | 既定 mode | artifact に既定 runtime mode を焼き込み、launchd plist へ mode を伝える |

依存順は P1 →（P2, P3 は並行）→ P4。P1 が無いと試す対象が存在しないため、これが起点である。
P2 は [#690](../../.usagi/issues/690-feat-cli-install-sh-binary-channel-v1-v2.md) を named channel へ
一般化する形で置き換える。#697 は試用の blocker ではないが、**stable artifact を出す前に必要**であり、
beta artifact の既定（`local`）を明示的に選ぶ意味でも先に入れておくのが望ましい。
