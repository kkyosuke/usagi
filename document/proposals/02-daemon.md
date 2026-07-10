# 提案: daemon（常駐プロセス）による agent ライフサイクルの TUI 非依存化

> [設計提案の目次](README.md) ｜ [ドキュメント目次](../README.md)

## 目次

- [要旨](#要旨)
- [背景と課題](#背景と課題)
- [現状のプロセス境界](#現状のプロセス境界)
- [目標アーキテクチャ](#目標アーキテクチャ)
- [コンポーネント設計](#コンポーネント設計)
- [設計上の難所と決定](#設計上の難所と決定)
- [段階的移行計画](#段階的移行計画)
- [リスクと未解決事項](#リスクと未解決事項)
- [クリーンアーキテクチャ上の位置づけ](#クリーンアーキテクチャ上の位置づけ)

## 要旨

usagi の TUI プロセスが単独で抱えている「agent / シェルの PTY 所有」「セッション監視」「委譲プロンプトの自動起動」を、**常駐プロセス `usagi daemon` へ移す**。TUI は daemon が所有する端末に **attach するクライアント**になる（tmux / abduco と同型）。これにより **TUI を閉じても agent は走り続け**、自律オーケストレーションが端末セッションの生存に縛られなくなる。

決定済みの方向: **PTY 所有を daemon に移す本格再設計**（軽量な監視のみの daemon ではなく）。

## 背景と課題

現状 usagi は共有メモリを持たず、`state.json` などのファイル＋ファイルロックで複数プロセス間の状態を共有する設計になっている（[data/](../data/README.md)）。状態の外部化は済んでいる一方で、**その状態を能動的に監視して実行に移す主体が TUI プロセスだけ**という一点に弱点が集中している。

結果、次がすべて「TUI が開いている間だけ」に縛られる（[04-orchestration.md](../04-orchestration.md) 参照）:

- `TerminalPool` が所有する agent / シェルの PTY は、TUI 終了時にプロセスごと破棄される。
- MCP 委譲（`session_prompt` / `session_delegate_issue`）で launch queue に積まれたプロンプトを拾って agent を spawn するのも TUI。
- ready / running / waiting / done のバッジ監視・通知・PR harvest も TUI スレッド。

自律オーケストレーション（root が session へ委譲し、agent が自走する）を掲げる以上、「人が端末を開いている間しか agent が動けない」制約は本質的な矛盾になっている。

## 現状のプロセス境界

```
        ┌─────────────────────────── TUI プロセス（開いている間だけ） ───────────────────────────┐
        │  event loop ── 描画・キー入力                                                          │
        │  TerminalPool ── PTY 所有（agent/shell 実プロセス）  ◀── 閉じると全部 kill              │
        │  state.json watcher ── 外部書き込み検知・反映                                          │
        │  session monitor ── phase+bell → バッジ・通知                                          │
        │  autostart ── launch/live queue を拾って spawn                                         │
        └───────────────────────────────────────────────────────────────────────────────────────┘
                        │ 読み書き（ファイル＋ロック、pid スタンプ）
                        ▼
   共有ストア: state.json / agent-state/ / agent-prompts/ / agent-live-panes/ / open-panes/ ...
                        ▲
                        │ 書くだけ（ファイルキューへ委譲）
        ┌───────────────┴──────────── MCP プロセス（agent 内に埋め込み） ────────────┐
        │  session_prompt / session_delegate_* ── PTY は触らず queue へ書く            │
        └──────────────────────────────────────────────────────────────────────────────┘
```

注目すべきは、**MCP がすでに「委譲＝ファイルに書く／実行＝TUI が拾う」で分離されている**こと。この境界の「実行」側を daemon に移すのが本提案の骨子で、共有ストア層はそのまま活かせる。

## 目標アーキテクチャ

```
   ┌──────────────── usagi daemon（常駐・端末非依存） ────────────────┐
   │  TerminalPool ── PTY 所有（agent/shell は daemon の子プロセス）   │
   │  session monitor ── phase+bell → 状態更新・通知                   │
   │  state.json watcher ── 外部書き込み検知                           │
   │  autostart ── launch/live queue を拾って spawn                    │
   │  IPC server（Unix domain socket）── attach/detach・vt100 stream   │
   └──────────────────────────────────────────────────────────────────┘
        ▲  attach（購読）          ▲ 書くだけ
        │  keystrokes（送信）       │
   ┌────┴──── TUI（クライアント） ──┐   ┌──┴── MCP（agent 内）──┐
   │  event loop ── 描画・入力       │   │  queue へ委譲          │
   │  daemon 端末に attach し vt100  │   └────────────────────────┘
   │  スナップショットを描く         │
   └─────────────────────────────────┘
```

役割の移動:

| 責務 | 現状 | 目標 |
|---|---|---|
| PTY 所有（agent/shell プロセス） | TUI | **daemon** |
| セッション監視（phase/bell → バッジ・通知） | TUI | **daemon** |
| state.json watcher / autostart | TUI | **daemon** |
| リソース計測 / PR harvest | TUI | **daemon** |
| 描画・キー入力・モーダル・フォーカス操作 | TUI | TUI（据え置き） |
| 委譲のキュー書き込み | MCP | MCP（据え置き） |

TUI が閉じても daemon が PTY と監視を持ち続けるため、**agent は走り続ける**。次に TUI を開くと daemon に再 attach して途中経過（vt100 スクリーン）をそのまま見られる。

## コンポーネント設計

### `usagi daemon` サブコマンド

- `usagi daemon start` / `stop` / `status` / `restart`。フォアグラウンド実行フラグ（`--foreground`）はデバッグ・テスト用。
- 起動時に PID / socket パスを `~/.usagi/daemon/`（グローバル）へ記録。多重起動はロックで防止（既存 `store_lock` の流儀を踏襲）。
- TUI 起動時に daemon が居なければ**自動起動（autospawn）**する（`git` CLI を叩くのと同様に、ユーザーは daemon を意識しない）。

### IPC（TUI ⇄ daemon）

- Unix domain socket（`~/.usagi/daemon/sock`）でフレーム化した JSON / バイナリを流す。Windows は named pipe（`portable-pty` と同様にプラットフォーム分岐）。
- 主なメッセージ:

  | 方向 | メッセージ | 内容 |
  |---|---|---|
  | TUI→daemon | `Attach { worktree }` | 端末購読を開始。直後に現在のスクリーン全体スナップショットを受け取る |
  | daemon→TUI | `Screen { cells / diff }` | vt100 のスクリーン状態（初回は全体、以降は差分） |
  | TUI→daemon | `Keys { bytes }` | キーストロークを PTY へ転送 |
  | TUI→daemon | `Resize { cols, rows }` | PTY のリサイズ |
  | TUI→daemon | `Detach` | 購読解除（PTY は生かす） |
  | daemon→TUI | `Sessions { ... }` | セッション一覧・バッジ状態の push（state.json watcher の結果） |

- **vt100 の権威は daemon 側**が持つ（`vt100::Parser` を daemon が保持）。TUI は描画専用にスクリーンを受け取る。これにより複数 TUI からの同時 attach でも表示が一貫する。

### 状態共有

- `state.json` / 各 worktree キー付きストアは**現状のまま**。daemon が主たる書き手／監視者になり、CLI・MCP・他プロセスの書き込みも従来どおりファイル経由で合流する。
- `agent-live-panes/` の「pid スタンプで生存 TUI を判定」の役割は、**daemon の attach テーブル**へ移す（誰が今その端末を見ているか、を daemon が権威的に知る）。

## 設計上の難所と決定

### 1. PTY 所有権の移動（最大の難所）

PTY をループのスタックに置くと離脱時に drop=kill される、という制約から現状 `TerminalPool` は「画面のライフタイム分だけ」所有している（`pool.rs` のヘッダコメント）。daemon 化ではこの所有権を daemon プロセスへ移し、**TUI 側は PTY を一切持たない**。TUI 終了で子プロセスが道連れにならないよう、daemon は独立したプロセスグループ／セッションリーダーとして PTY を fork する。

### 2. vt100 ストリーミングと差分

- 初回 attach で全スクリーンを送り、以降はダーティセル差分を push（帯域と描画コストを抑える）。
- スクロールバック（履歴）をどこまで daemon が保持し、attach 時にどこまで転送するかは要検討（当面は現在スクリーン＋固定行数のバックログ）。

### 3. マルチクライアント

- 同一端末に複数 TUI が attach しうる（別ウィンドウ／別マシン via SSH）。入力は最後に操作したクライアント優先か、明示ロックか。当面は**入力は全 attach クライアントから受理、表示は全員同期**（tmux のデフォルト挙動）とする。

### 4. daemon のライフサイクルと孤児

- daemon がクラッシュした場合、子 agent は孤児化する。再起動時に daemon は `state.json` と PTY プロセスの生存を突き合わせて**再 adopt** する（プロセス生存確認＋pid 記録）。
- 逆に、明示的な `daemon stop` は「走行中 agent があるなら確認する／`--force` で kill」。長時間 idle でも agent が生きていれば daemon は落とさない。

### 5. 通知の発火主体

- デスクトップ通知は daemon が発火できる（TUI 非依存で waiting/done を通知するのが daemon 化の価値の一つ）。TUI が attach 中はどちらが出すか（二重通知の抑止）を attach テーブルで調停する。

## 段階的移行計画

いきなり全部を移すのではなく、共有ストア層が既に多プロセス対応である利点を使って**振る舞いを保ったまま**段階移行する。Epic は issue ストアの #159、各段は子 issue で追跡する。

1. **daemon スケルトン / 制御プレーン**（#160・実装済み）: `usagi daemon start/stop/status`（`serve` は隠しサブコマンド）と、ファイルベースのレコード（`<data-dir>/daemon/daemon.json`）・stop マーカー・単一インスタンスロックだけ。まだ PTY も監視も所有しない。IPC socket・autospawn は次段以降。
2. **監視の移設**（#161・一部実装済み）: session monitor（phase 由来の activity 集約）を daemon へ移す。daemon が全登録ワークスペースのセッションを毎ティック走査し、各セッションの [`SessionActivity`] スナップショットを `<data-dir>/daemon/sessions.json` に保存、`usagi daemon status` で可視化する。**通知の発火は保留**（bell 信号と PTY は TUI 所有のため、今 daemon から通知すると TUI 併用時に二重通知になる。通知調停は Step 4）。**daemon→TUI の push は Step 3 の IPC socket と一緒**に入れる（現状は TUI が引き続き自前の watcher を使う）。
3. **PTY 所有の移設（核心）**: `TerminalPool` を daemon 側へ移し、TUI を attach クライアント化。`Attach`/`Screen`/`Keys`/`Resize` を実装。ここで初めて「閉じても走り続ける」が成立。
   - **3a. IPC + attach プロトコルの土台**（#162・実装済み）: daemon が Unix domain socket（`<data-dir>/daemon/sock`）を開き、クライアントは `subscribe`/`list_sessions` で監視スナップショットを取得・購読できる。監視ティックが変化を検知すると購読者へ `Sessions` を push する。プロトコル（メッセージ型・長さ前置フレーム・購読レジストリ・dispatch）は純粋・ユニットテスト済みで、socket サーバは合成ルート（`main.rs`）が束ねる。次スライスで配信内容を `Sessions` から `Screen`（PTY 画面）へ置き換える。
   - **3b. PTY 所有の移設**: `TerminalPool` を daemon へ移し、`Attach`/`Screen`/`Keys`/`Resize` と vt100 の権威を daemon に持たせる。TUI は attach して描画するクライアントになる。
     - **3b-1. daemon が PTY を所有**（#164・実装済み）: IPC に `spawn`/`kill` を追加し、daemon が worktree ごとの端末（`PtySession`）を**自プロセスの子として所有**する。所有権が daemon にあるため、要求したクライアントが切断しても端末プロセスは生き続ける（e2e で実証）。**単一端末で「閉じても走り続ける」が成立**。
     - **3b-2. Screen ストリーミング**（#165・実装済み）: IPC に `attach`/`detach` を追加し、daemon が worktree ごとの端末の vt100 画面（`contents_formatted` の replay バイト列）を attach したクライアントへ配信する。attach 時に現在画面を即送信し、以降は毎ティック画面世代（`generation`）の変化を検知して push。daemon が vt100 の権威を持つ。
     - **3b-3. TUI を attach クライアント化**: `Keys`/`Resize` を追加し、`TerminalPool` を daemon 所有端末への attach に置き換える。TUI は daemon 端末に接続して描画・入力する。
4. **孤児 adopt・マルチクライアント・通知調停**の仕上げ。
5. **ドキュメント畳み込み**: 挙動確定後、本提案を [04-orchestration.md](../04-orchestration.md) と [02-architecture.md](../02-architecture.md) / [data/](../data/README.md) へ畳み込み、proposals はリンクスタブ化。

各段は独立して PR 可能で、カバレッジ 100% を維持する（IO は注入し、IPC・PTY・実プロセスは合成ルートで束ねる — [06-conventions.md](../06-conventions.md) の DI 方針）。

## リスクと未解決事項

- **複雑性の増加**: プロセス境界・IPC・attach プロトコル・孤児回収は、単一プロセス TUI より格段に難しい。テスト戦略（IPC を注入してユニットで検証／実 socket は薄いオーケストレータに閉じる）を段階 1 から確立する必要がある。
- **カバレッジ 100% の維持**: IPC サーバ・PTY fork・socket は「実 IO そのもの」に寄るため、ロジック（メッセージ処理・attach テーブル・差分計算）をハンドラから分離して注入可能にする。`COVERAGE_IGNORE` に逃がさない。
- **Windows 対応**: named pipe とプロセスグループの扱いが Unix と異なる。`portable-pty` の分岐に倣う。
- **後方互換**: daemon 不在（古い挙動を望むユーザー）でも TUI が最低限動くフォールバックを残すか、daemon 必須にするかは設定方針の決定が要る。
- **リソース**: 常駐プロセスの CPU/メモリ。監視ポーリング間隔は現状の TUI 値（500ms / 200ms）を引き継ぎ、attach クライアント数 0 のときは間引く。

## クリーンアーキテクチャ上の位置づけ

- daemon 本体は**新しい presentation**（`presentation/daemon/`）。TUI と同じく usecase を呼ぶ入口の一つになる。
- PTY・IPC socket・プロセス管理は **infrastructure**（`pty.rs` は流用、`ipc.rs` を追加）。
- セッション監視・autostart・状態遷移は既存 usecase / domain を再利用し、駆動主体だけが TUI から daemon へ移る。依存方向（`domain → usecase → infrastructure ← presentation`）は不変。

---

関連: [04-orchestration.md](../04-orchestration.md)（セッションのライフサイクル・ペイン復旧・queued-prompt autostart）／[02-architecture.md](../02-architecture.md)（層構成・ストア一覧）／[data/](../data/README.md)（永続化フォーマット）。
