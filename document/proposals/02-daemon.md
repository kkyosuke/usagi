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

- Unix domain socket（`~/.usagi/daemon/sock`。owner 限定の 0600 — `Spawn` が解決済み workspace env を運ぶため）でフレーム化した JSON を流す。Windows は named pipe（`portable-pty` と同様にプラットフォーム分岐）が将来課題で、現状 IPC は Unix 限定（非 Unix の TUI はローカル PTY で従来どおり動く）。
- 端末は daemon が spawn 時に採番する **terminal id** で指す（1 worktree に agent＋terminal 複数タブが同居するため、worktree パスだけでは端末を特定できない）。実装済みのメッセージ:

  | 方向 | メッセージ | 内容 |
  |---|---|---|
  | TUI⇄daemon | `Hello { build }` | terminal 操作前に executable generation を相互確認する。不一致（`cargo run` の再ビルド前 daemon を含む）なら手動の新規ペインはローカル PTY へフォールバックし、保存済み terminal の復旧と queued prompt の自動起動は Agent の二重起動を避けるため local fallback しない |
  | TUI→daemon | `Spawn { worktree, command?, env, cols, rows, scrollback }` | 新しい daemon 所有端末を起動（agent は `command` を shell 引数で実行）。応答は `Spawned { terminal, worktree, pid }` |
  | TUI→daemon | `Attach { terminal, worktree }` | 端末購読を開始（worktree の照合つき — 古い保存 id が別 worktree の端末に化けない）。応答は `Attached { terminal, pid }`＋viewport `Screen` |
  | TUI→daemon | `Scrollback { terminal, offset }` | daemon 側 scrollback から `offset` 行戻った viewport を要求する。応答は clamped offset つき `Screen` |
  | daemon→TUI | `Screen { terminal, contents, scrollback }` | vt100 viewport の replay バイト列（attach 直後、scrollback 要求、クライアントがバックログから脱落した時の resync、scrollback 表示中の追従 snapshot） |
  | daemon→TUI | `Output { terminal, data }` | live viewport の前回 push 以降の**生 PTY 出力バイト列**（差分）。クライアントは scrollback なしの bounded parser に食わせ、履歴本文は保持しない |
  | daemon→TUI | `Exited { terminal }` | 端末プロセスの終了（最終 `Output` を流し切ってから push） |
  | TUI→daemon | `Keys { terminal, data }` | キーストロークを PTY へ転送 |
  | TUI→daemon | `Resize { terminal, cols, rows }` | PTY のリサイズ |
  | TUI→daemon | `Detach { terminal }` / `Kill { terminal }` | 購読解除（PTY は生かす）／明示的な終了（応答は `Killed`） |
  | daemon→TUI | `Sessions { ... }` | セッション一覧・バッジ状態の push（state.json watcher の結果） |

- **vt100 の権威は daemon 側**が持つ（`vt100::Parser` を daemon が保持）。各端末の生出力は容量制限付き **output backlog**（純粋な `OutputBacklog` リング）に tee され、live viewport の attach クライアントへ差分配信する。TUI は scrollback なしの bounded parser で live viewport を描画し、履歴表示は `Scrollback` 要求に対する daemon snapshot で復元するため、scrollback 本文を client ごとに重複保持しない。
- 同一端末への複数 attach は全クライアントから `Keys` を受理し、出力は全 attach クライアントへ同じ backlog から同期配信する。`Resize` は最後に届いた要求を daemon 側 PTY のサイズとして採用する。
- daemon のループはクライアント接続中 15ms / 無人時 500ms の 2 段 tick（タイプ echo のレイテンシと常駐コストの両立）。

### 状態共有

- `state.json` / 各 worktree キー付きストアは**現状のまま**。daemon が主たる書き手／監視者になり、CLI・MCP・他プロセスの書き込みも従来どおりファイル経由で合流する。
- `agent-live-panes/` の「pid スタンプで生存 TUI を判定」の役割は、**daemon の attach テーブル**へ移す（誰が今その端末を見ているか、を daemon が権威的に知る）。
- daemon は `<data-dir>/daemon/sessions.json` に session activity snapshot を、`<data-dir>/daemon/terminals.json` に daemon 所有端末の terminal id / worktree / pid を保存する。`terminals.json` は daemon 異常終了後の orphan adopt と正常 stop 時の process group 回収に使う。

## 設計上の難所と決定

### 1. PTY 所有権の移動（最大の難所）

PTY をループのスタックに置くと離脱時に drop=kill される、という制約から現状 `TerminalPool` は「画面のライフタイム分だけ」所有している（`pool.rs` のヘッダコメント）。daemon 化ではこの所有権を daemon プロセスへ移し、**TUI 側は PTY を一切持たない**。TUI 終了で子プロセスが道連れにならないよう、daemon は独立したプロセスグループ／セッションリーダーとして PTY を fork する。

### 2. vt100 ストリーミングと差分

- **決定（3b-4 で実装）**: 差分は**生の PTY 出力バイト列**をそのまま流す（セル差分は組まない）。初回 attach と、クライアントが容量制限付き output backlog から脱落した時だけ、全画面（`state_formatted`: 内容＋入力モード）を送って resync する。生バイト列はローカルで所有していた頃と同じものなので、ベル・カーソル形状（DECSCUSR）・bracketed paste・スクロールバックの成長まで追加のプロトコルなしで再現される。
- attach 時点より前のスクロールバックは転送しない（daemon 側には残る）。attach 後の履歴は Output 差分の再生でクライアント側にも積もる。

### 3. マルチクライアント

- **決定（Step 4 で実装）**: 入力は全 attach クライアントから受理し、表示は全員同期する（tmux のデフォルト挙動）。resize の競合は最後に daemon へ届いた `Resize` を採用し、そのサイズで PTY と daemon 側 vt100 grid を更新する。

### 4. daemon のライフサイクルと孤児

- **決定（Step 4 で実装）**: daemon は spawn / kill / exit のたびに `<data-dir>/daemon/terminals.json` を更新する。再起動時は保存された pid を `process_alive` で突き合わせ、生存している terminal id を registry へ戻す。異常終了後は以前の PTY master fd を復元できないため、adopted terminal は画面 stream へ再 attach できないが、terminal id の再利用を避け、`Kill` / `daemon stop` で process group を回収できる。
- 正常な `daemon stop` は daemon が所有する live PTY を drop し、adopted pid も process group kill してから `terminals.json` を空にする。

### 5. 通知の発火主体

- **決定（Step 4 で実装）**: daemon が session activity snapshot の差分から waiting / done 通知を発火する。waiting は attach 中でも通知し、done は対象 worktree に attach クライアントがいる場合は抑制する。`settings.json` の `notifications_enabled` が false の場合は daemon 側通知も発火しない。

## 段階的移行計画

いきなり全部を移すのではなく、共有ストア層が既に多プロセス対応である利点を使って**振る舞いを保ったまま**段階移行する。Epic は issue ストアの #159、各段は子 issue で追跡する。

1. **daemon スケルトン / 制御プレーン**（#160・実装済み）: `usagi daemon start/stop/status`（`serve` は隠しサブコマンド）と、ファイルベースのレコード（`<data-dir>/daemon/daemon.json`）・stop マーカー・単一インスタンスロックだけ。まだ PTY も監視も所有しない。IPC socket・autospawn は次段以降。
2. **監視の移設**（#161・一部実装済み）: session monitor（phase 由来の activity 集約）を daemon へ移す。daemon が全登録ワークスペースのセッションを毎ティック走査し、各セッションの [`SessionActivity`] スナップショットを `<data-dir>/daemon/sessions.json` に保存、`usagi daemon status` で可視化する。**通知の発火は保留**（bell 信号と PTY は TUI 所有のため、今 daemon から通知すると TUI 併用時に二重通知になる。通知調停は Step 4）。**daemon→TUI の push は Step 3 の IPC socket と一緒**に入れる（現状は TUI が引き続き自前の watcher を使う）。
3. **PTY 所有の移設（核心）**: `TerminalPool` を daemon 側へ移し、TUI を attach クライアント化。`Attach`/`Screen`/`Keys`/`Resize` を実装。ここで初めて「閉じても走り続ける」が成立。
   - **3a. IPC + attach プロトコルの土台**（#162・実装済み）: daemon が Unix domain socket（`<data-dir>/daemon/sock`）を開き、クライアントは `subscribe`/`list_sessions` で監視スナップショットを取得・購読できる。監視ティックが変化を検知すると購読者へ `Sessions` を push する。プロトコル（メッセージ型・長さ前置フレーム・購読レジストリ・dispatch）は純粋・ユニットテスト済みで、socket サーバは合成ルート（`main.rs`）が束ねる。次スライスで配信内容を `Sessions` から `Screen`（PTY 画面）へ置き換える。
   - **3b. PTY 所有の移設**: `TerminalPool` を daemon へ移し、`Attach`/`Screen`/`Keys`/`Resize` と vt100 の権威を daemon に持たせる。TUI は attach して描画するクライアントになる。
     - **3b-1. daemon が PTY を所有**（#164・実装済み）: IPC に `spawn`/`kill` を追加し、daemon が worktree ごとの端末（`PtySession`）を**自プロセスの子として所有**する。所有権が daemon にあるため、要求したクライアントが切断しても端末プロセスは生き続ける（e2e で実証）。**単一端末で「閉じても走り続ける」が成立**。
     - **3b-2. Screen ストリーミング**（#165・実装済み）: IPC に `attach`/`detach` を追加し、daemon が worktree ごとの端末の vt100 画面（`contents_formatted` の replay バイト列）を attach したクライアントへ配信する。attach 時に現在画面を即送信し、以降は毎ティック画面世代（`generation`）の変化を検知して push。daemon が vt100 の権威を持つ。
     - **3b-3. daemon 端末への入力（`Keys`/`Resize`）**（#166・実装済み）: IPC に `Keys`（入力バイト列）と `Resize` を追加し、daemon が該当端末の `PtySession` へ書き込み／リサイズする。入力→端末→`Screen` 出力の往復が成立（e2e で「タイプしたコマンドの出力が画面に現れる」ことを実証）。これで daemon 端末の I/O が IPC 越しに完結する。
     - **3b-4. TUI を attach クライアント化**（#167・#199・実装済み）: TUI 起動時に daemon を autospawn し、ペインの実体を `PaneBackend`（daemon 所有端末への attach クライアント `DaemonTerminal` ／ フォールバックのローカル `PtySession`）に差し替えた。端末は daemon 採番の terminal id で指し（`Spawn` が `command` / `env` / `geometry` / `scrollback` を運ぶ）、attach 後の live viewport は生 PTY 出力の `Output` 差分を scrollback なしの bounded vt100 parser で再生する。履歴表示は `Scrollback` 要求に対して daemon が `Screen` snapshot を返すため、TUI は full scrollback parser を持たない。detach・TUI 終了は購読解除のみで端末プロセスは daemon に残り、タブの明示クローズ／`session remove` が `Kill` を送る。open-panes スナップショットに terminal id を保存し、次回起動はまず**再 attach**（走行中の agent を画面ごと引き継ぐ）、daemon が id を知らないときだけ従来どおり再 spawn する。daemon 不通時はローカル PTY へフォールバック（エラーログに記録。非 Unix も常にローカル）。**「TUI を閉じても agent が走り続ける」が TUI 経由で成立**（実端末の PTY 駆動で検証済み: ペインを開いて入力 → detach → TUI 終了 → シェル生存 → 再起動で画面ごと復元）。
4. **孤児 adopt・マルチクライアント・通知調停**（#168・実装済み）: daemon が `terminals.json` を保存し、異常終了後に生存 pid を adopted terminal として registry へ復元する。複数 attach は全クライアント入力・全員同期表示で扱い、resize は最後の要求を採用する。daemon は waiting / done の activity 差分からデスクトップ通知を発火し、attach 中の done 通知だけを抑制する。
5. **ドキュメント畳み込み**: 挙動確定後、本提案を [04-orchestration.md](../04-orchestration.md) と [02-architecture.md](../02-architecture.md) / [data/](../data/README.md) へ畳み込み、proposals はリンクスタブ化。

#205 では terminal の `Spawn` / `Attach` より先に `Hello { build }` を往復する executable-generation handshake を追加した。build は package version に加えて実行ファイルの device / inode / size / mtime を含むため、version が同じ `cargo run` の再ビルドも区別する。不一致や `Hello` を持たない旧 daemon では terminal 操作を送らず、手動の新規ペインは既存 daemon とその Agent を止めずにローカル PTY へフォールバックする。保存済み terminal id の復旧と queued prompt の自動起動は、旧 Agent が生存したまま別 Agent を起動するのを防ぐため local fallback せず、失敗または再試行として記録する。

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
