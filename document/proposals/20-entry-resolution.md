# 20. Welcome の主語を入れ替えて Open / New / Recent を回収する

> [設計提案一覧](README.md) ｜ 関連する現在仕様: [3. TUI](../03-tui.md#画面と入力) / [1. プロジェクト概要](../01-overview.md#cli) ｜ 関連提案: [15-session-garden.md](15-session-garden.md) / [17-multi-workspace-daemon.md](17-multi-workspace-daemon.md)

> **Status:** 提案中（未実装）。本書は Welcome を**残したまま**、使われていない Open / New / Recent を Welcome 自身の中へ畳み込む target design である。
> 現在のビルドの挙動は [3. TUI](../03-tui.md) と [11. キーバインド](../11-keybindings.md) を正本とする。
>
> **Baseline:** 原版 commit `573dc395df99270f53baaca1bcc4b8d5861b0823`（2026-09-15）。現在仕様は上記の TUI とキーバインドを参照する。

## 目的と制約

Welcome の Open / New / Recent が日常的に使われていない。一方で **Welcome 画面そのものは残す**。
したがって解は「画面を削る」ではなく、**Welcome の主語を入れ替えて 3 項目をその中へ回収する**ことになる。

| 制約 | 内容 |
|---|---|
| Welcome を残す | `usagi`（引数なし）の入口、`Ctrl-Q` → `w` の戻り先、マスコットと splash の顔を保つ |
| entry 面は daemon 非依存 | 最初の 1 フレームは registry と Recent だけで描き、daemon の起動・接続を待たない |
| SSoT を割らない | `+ Open` overlay、Session Garden、Home の投影を Welcome 側へ複製しない。使うなら同じ投影を読む |

## 問題の構造

Welcome が使われない理由は「画面の出来」ではなく、**主語**と**重複**である。

現在の Welcome の主語は「何をしますか？」で、答えとして Open / New / Config / Quit の 4 択と Recent カード 3 枚を出す。
しかし利用者が起動時に本当に持っている問いは「**前回の続きはどうなっている？**」であり、Welcome はそれに答えていない。
そのうえ 4 択のうち 3 つは Home 側に等価物がある。

| Welcome の項目 | Home の等価物 | CLI の等価物 | 判定 |
|---|---|---|---|
| Open（登録済み一覧・filter・Unite 選択） | `+ Open` overlay（filter・`Space` 複数選択） | — | 重複 |
| Recent（カード 3 枚） | project tab deck そのもの | `usagi open` | 起動直後だけ意味を持つ |
| New / Existing（既存ディレクトリ登録） | `+ Open` overlay の Directory 入力 | `usagi open <path>` | 重複 |
| New / Clone（git clone して登録） | なし | なし | **Welcome 固有** |
| Config | Overview の `config`（overlay modal） | `usagi config` | 重複 |
| Quit | `Ctrl-Q` の exit prompt | — | 重複 |

**Welcome 固有の機能は Clone だけである**。Welcome を残すと決めた以上、重複の解消先は Home 側ではなく
**Welcome 側の情報設計**になる。つまり 4 択メニューという形をやめ、3 項目を格の違う 3 か所へ置き直す。

## 設計判断

主語を「何をしますか？」から「**どこへ戻るか・今どうなっているか**」へ入れ替え、3 項目を次の格へ移す。

| 項目 | 回収先 | 格 |
|---|---|---|
| Open | **画面本体のリストへ昇格**。Welcome が登録済み workspace の一覧そのものになる | 主 |
| Recent | **並び順と `Continue` 行へ溶かす**。カード 3 枚という別表現をやめる | 既定値 |
| New | **footer の 1 キーへ降格**（`n`）。Clone / Existing の画面は現状のまま残す | 従 |
| Config | **footer の 1 キーへ降格**（`c`） | 従 |

### 1. `Continue` 行を既定選択にする

最上段に `Continue` 行を置き、**起動直後のカーソルをそこに合わせる**。Enter 1 打で前回の続きに戻る。

`Continue` が指す対象の決め方（registry と Recent だけで決まり、daemon に問い合わせない）:

```text
Welcome を開く
   |
   +-- cwd が登録済み workspace の内側 ------> その workspace
   +-- 直近の Recent（Unite ならその deck 構成ごと）-> その deck
   `-- 登録が 0 件 --------------------------> Continue 行を出さず New を既定選択にする
```

**起動時に自動で workspace を開く経路は作らない**。Welcome は必ず表示し、決定は Enter という利用者の 1 操作に残す。
これで「毎回同じ問いに答える」コストは 1 打鍵まで下がり、画面は残る。ゼロ打鍵で入りたい場合の入口は
既存の `usagi open [path]` が既に担っている。

### 2. Open を画面本体へ昇格し、メニュー列を廃止する

左のメニュー列（Open / New / Config / Quit の 4 行）と右の Recent カード列という 2 カラムをやめ、
**filter 1 行＋ workspace リスト 1 本**にする。リストは最近順、続けて名前順。
`views/open.rs` の Filter・`Tab` の Single / Unite 切り替え・`Space` の複数選択は、画面を移さず Welcome 上でそのまま使う。
Open は独立した画面としては無くなるが、機能は 1 つも落ちない。

```text
              (mascot)
               USAGI

   ▸ Continue   usagi + AccelHack          3 sessions · 1 needs attention
   ─────────────────────────────────────────────────────────────────────
     usagi        ~/git/…/usagi             2 sessions · 1 needs attention
     AccelHack    ~/git/…/AccelHack         1 session
     dotfiles     ~/git/…/dotfiles          —
   ─────────────────────────────────────────────────────────────────────
   /: filter   Tab: unite   n: new   c: config   q: quit   Ctrl-?: help
```

### 3. 空いた面積へ daemon-optional な status を置く

メニュー列と カードを畳んで空いた右側に、workspace ごとの **session 数と `N needs attention`** を出す。
これが「Welcome を毎回見る価値」に相当し、Open / New / Recent が占めていた面積の回収先になる。

制約を守るための規則:

| 規則 | 内容 |
|---|---|
| 最初のフレーム | registry と Recent だけで描く。status 列は空（`—`）で出し、後から埋める |
| 読み方 | Garden の inactive project 観測と同じ `AgentWorkspaceObservation` を使う。workspace ごとに daemon が自分の record を filter して答えるため、その workspace の tenant へ接続し直さない |
| いつ読むか | Welcome が前面にある間だけ。1 round ずつ直列で、成功後 1 秒・空振り後 5 秒。1 round の上限 16 件 |
| daemon が居ないとき | 何も表示せず、エラーも出さず、**daemon を起動しない**。status は「あれば出る」付加情報に留める |
| 数え方 | attention 件数の定義は [Garden Action Center](../03-tui.md#garden-action-center) を正本として共有し、Welcome 側で数え直さない |

## キー操作の before / after

| キー | 現在 | 提案後 |
|---|---|---|
| `↑` / `↓`、`j` / `k` | メニュー 4 項目の移動 | workspace リストの移動（`Continue` を含む） |
| `Enter` | 選択したメニュー項目 | 選択した workspace / `Continue` を開く |
| `1`…`3` | Recent カード | 廃止（リストの並び順へ溶ける） |
| `o` | Open 画面へ | 廃止（画面が本体になる） |
| `e` | New 画面へ | `n`（footer 表記と一致させる） |
| `c` / `q` | Config / Quit | 変更なし |
| `/` | なし | filter へフォーカス |
| `Tab` / `Space` | なし（Open 画面の機能） | Single / Unite 切り替えと複数選択 |

## 段階

| 段階 | 内容 | 主な変更点 | 効果 |
|---|---|---|---|
| 1 | `Continue` 行と既定選択 | `views/welcome.rs` の選択初期値と 1 行追加、cwd 判定 | 起動が Enter 1 打になる。Recent カードはまだ残せる |
| 2 | 1 リスト化 | `views/welcome.rs` と `views/open.rs` の統合、メニュー列と Recent カードの廃止、footer 整理 | Open / Recent の重複が消え、画面が 1 つ減る |
| 3 | status 列 | 観測 lane の Welcome 版（bounded round・daemon 非依存の縮退） | Welcome に毎回見る理由ができる |

段階 1 は描画とカーソル初期値だけで完結し、backend を増やさない。段階 2 までで「使われない 3 項目」は無くなる。
段階 3 は daemon 依存を**任意の付加情報**として足すため、失敗しても Welcome は今と同じ情報量で成立する。

## 却下した代替案

| 案 | 却下理由 |
|---|---|
| 起動時に workspace を解決して Welcome を出さない | Welcome を残すという決定に反する。決定は Enter 1 打へ縮めるに留める |
| Welcome を消して Home の空 deck へ統合 | 同上。加えて 0 件 deck の Home は active tab を持てず、特殊な Home を新設することになる |
| Clone を `+ Open` overlay へ移し、New も畳む | Clone は Welcome 固有で重複していない。重複していない機能を移しても総量は減らず、overlay のモードだけが増える |
| Welcome 上に Garden を別実装で描く | 同じ投影の二重実装になり SSoT が割れる。Welcome では 1 行の status に留め、庭は Garden の 1 か所に保つ |
| 起動挙動（自動で開く / Welcome を出す）を Global 設定にする | 既定が Enter 1 打なら設定する動機が無く、entry の分岐と設定 surface だけが増える |
| cwd が未登録なら暗黙に登録して `Continue` に出す | 任意のディレクトリで `usagi` を打つだけで registry が汚れる。登録は `usagi open` の明示操作に留める |

## 未決事項

- `Continue` が Unite deck を指すときの表記（`usagi + AccelHack` のような連結か、保存済み deck 名を持たせるか）。
- 段階 3 の status を Welcome だけでなく `+ Open` overlay の候補行にも出すか（同じ projection を共有できる）。
- Welcome が idle になったとき Garden を開くか。Home と同じ idle 契約を entry 面へ広げると
  「entry は daemon 非依存」の原則と衝突するため、開くとしても daemon が既に居る場合に限る。
