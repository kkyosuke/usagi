# 11. キーバインド

> [ドキュメント目次](README.md) ｜ ← 前へ [10. session role](10-session-roles.md)

TUI のキーバインド、入力所有権、割り振り規則の正本である。画面遷移と各操作の詳細は
[3. TUI](03-tui.md)を参照する。本書はキーボード入力だけを対象とし、クリック、ドラッグ、ホイールは
TUI 仕様を正本とする。

## 目次

- [割り振り規則](#割り振り規則)
- [全画面共通](#全画面共通)
- [workspace 共通コマンド](#workspace-共通コマンド)
- [entry 画面](#entry-画面)
- [workspace 画面](#workspace-画面)
- [modal と drawer](#modal-と-drawer)
- [テキスト編集](#テキスト編集)
- [live terminal](#live-terminal)

## 割り振り規則

| 種別 | 規則 |
|---|---|
| ヘルプ | `Ctrl-?`（portable alias: `Ctrl-/`）は全画面、plain `?` は前面に入力modal / drawerがないworkspace、`Ctrl-O ?` はlive paneで、現在の最前面surfaceに有効なキーだけを表示する |
| workspace 共通操作 | `Ctrl-O` を leader とする 2 打鍵へ集約し、2 打目は 1 action だけを持つ |
| tab | `[` / `]` は前 / 次の選択、`{` / `}` は前 / 次への並べ替えとする |
| 対象の除去 | `Ctrl-X` は選択中の対象を安全に remove / detach / dismiss する。plain `x` / `X` に副作用を割り当てない |
| tab の終了 | `Ctrl-O x` は現在の pane tab を閉じる。`x` の「現在対象を閉じる」という意味を維持し、session remove とは入力 scope を分ける |
| 強制削除 | 1 打鍵で実行しない。safe remove が拒否された session は確認 modal、command は明示的な `--force` を使う |
| modal 内操作 | `Enter` は決定、`Esc` は取消、矢印は選択、`Tab` は focus / mode 移動として再利用する |
| 文字入力 | plain letter は入力欄と live terminal へ渡す。workspace 共通操作に plain letter を使わない |

同じ scope で異なる action が同じ入力を持つ状態を衝突とする。`Ctrl-X` の remove / detach / dismiss のように、
異なる画面でも「現在選択している対象を一覧から除く」という同じ意味を保つ割り当ては同じ action family とする。
`Enter` / `Esc` / 矢印などの標準 modal 操作も同じ意味で再利用する。

## 全画面共通

| 入力 | 動作 |
|---|---|
| `Ctrl-?` / `Ctrl-/` | 現在の画面、modal、drawerで使用できるコマンドを表示 |
| help表示中の `Ctrl-?` / `Ctrl-/` / `Esc` | helpを閉じる |

従来型terminalが `Ctrl-/` または `Ctrl-Shift-/` を raw `0x1f` として送る場合も同じhelpとして扱う。
`0x7f` はterminalによってBackspaceを表すためhelpには割り当てない。helpは最前面の入力ownerになり、表示中の
その他の入力を背面のフォームやlive terminalへ渡さない。workspaceではdaemon更新とterminal観測を止めずに
表示を更新する。

## workspace 共通コマンド

`Ctrl-O` の次の入力は 1 秒以内に行う。workspace の route・modal・drawer・pane の有無にかかわらず、
2打目はすべて `Ctrl` の有無を同一視するため、たとえば
`Ctrl-O p` と `Ctrl-O Ctrl-P`、`Ctrl-O [` と `Ctrl-O Ctrl-[` はそれぞれ同じ操作になる。
従来型terminalが `Ctrl-[` / `Ctrl-]` を raw `0x1b` / `0x1d` として送る場合も、前 / 次のtabとして扱う。

| 入力 | action | 動作 |
|---|---|---|
| `Ctrl-O +` | OpenWorkspace | workspace 追加 |
| `Ctrl-O 0` | OpenWorkspaceSwitcher | project / session finder |
| `Ctrl-O 1` … `9` | ActivateWorkspace | project tab を番号で選択 |
| `Ctrl-O ?` | KeyboardHelp | live paneで使えるキーボードショートカットを表示 |
| `Ctrl-O o` | Switch | Switchへ戻る |
| `Ctrl-O a` | OpenCloseupModal | 選択中targetのAction |
| `Ctrl-O [` | PreviousTab | 前のpane tab |
| `Ctrl-O ]` | NextTab | 次のpane tab |
| `Ctrl-O {` | MoveTabPrevious | pane tabを前へ並べ替え |
| `Ctrl-O }` | MoveTabNext | pane tabを次へ並べ替え |
| `Ctrl-O p` | OpenPullRequests | Pull Request一覧 |
| `Ctrl-O v` | OpenPreview | Markdown Preview |
| `Ctrl-O d` | OpenDecisions | pending Decision一覧 |
| `Ctrl-O s` | OpenNotes | Scratchpad |
| `Ctrl-O ,` | OpenGarden | Session Garden |
| `Ctrl-O g` | Director | Director drawer |
| `Ctrl-O w` | WorkRuns | Work Runs |
| `Ctrl-O t` | WorkspaceTerminal | workspace root Shell drawer |
| `Ctrl-O z` | WorkspaceTerminalFullHeight | Shell drawerの高さ切替 |
| `Ctrl-O n` | DirectorNew / NewRootTerminal | Director New。Shell選択中は新しいterminal tab |
| `Ctrl-O x` | CloseTab | 選択中pane tabの終了／取消／dismiss |
| `Ctrl-O r` | ResumeTab | interrupted Agent tabの再開 |
| `Ctrl-O ↑` | ScrollUp | retained outputを1行上へ |
| `Ctrl-O ↓` | ScrollDown | retained outputを1行下へ |
| `Ctrl-O End` | ScrollBottom | live bottomへ戻る |

未割り当ての follow-up は消費して leader をresetする。leaderを伴わない文字・数字・記号はlive terminalへ渡す。

## entry 画面

| 画面 | 入力 | 動作 |
|---|---|---|
| Welcome | `↑` / `k`、`↓` / `j` | 前 / 次の項目 |
| Welcome | `Enter` | 選択項目を開く |
| Welcome | `o` / `e` / `c` / `q` | Open / New / Config / Quit |
| Welcome | `1` … `3` | Recent cardを開く |
| Welcome | `Esc` / `Ctrl-C` / `Ctrl-Q` | Quit |
| Open | `↑` / `↓` | workspace選択 |
| Open | 文字 / paste / `Backspace` / `Delete` | filter編集 |
| Open | `Tab` | Single / Unite |
| Open | `Space` | Unite対象をmark |
| Open | `Enter` | open |
| Open | `Ctrl-X` | 選択workspaceの登録解除確認 |
| Open | `C` | 存在しない登録のcleanup確認 |
| Open | `Esc` | Welcomeへ戻る |
| 登録解除確認 | `←` / `→` / `Tab` | Confirm / Cancel |
| 登録解除確認 | `y` / `Y` / `Enter` | confirm |
| 登録解除確認 | `n` / `N` / `Esc` | cancel |
| 登録cleanup確認 | `y` / `Enter` | confirm |
| 登録cleanup確認 | `n` / `Esc` | cancel |
| New | `↑` / `↓` | field移動 |
| New | `Tab` | directory補完 |
| New | `←` / `→` | modeまたはcaret移動 |
| New | `Enter` | create |
| New | `Esc` | Welcomeへ戻る。create待機中は画面上の待機をcancel |
| Config | `↑` / `k`、`↓` / `j` | field移動 |
| Config | `←` / `h`、`→` / `l` | 値変更 |
| Config | `Enter` | editor / picker / save |
| Config | `Esc` | 戻る |
| Team picker | `←` / `h`、`→` / `l` | template card |
| Team picker | `↑` / `k`、`↓` / `j` | template / no-template |
| Team picker | `Enter` / `Esc` | apply / cancel |
| Environment editor | `Enter` | 改行 |
| Environment editor | `Tab` | workspace editorのtextarea / Save移動 |
| Environment editor | `Ctrl-S` | 保存 |
| Environment editor | 矢印、`Home` / `End` | caret移動 |
| Environment editor | 文字 / paste / `Backspace` / `Delete` | source編集 |
| Environment editor | `Esc` | cancel |
| Roles editor | `Tab` | global / workspace scope |
| Roles editor | `Ctrl-S` | 保存 |
| Roles editor | `↑` / `↓`、`PgUp` / `PgDn` | 行 / page移動 |
| Roles editor | 文字 / paste / `Enter` / `Backspace` | source編集 |
| Roles editor | `Esc` | 閉じる |

entry画面の `Ctrl-C` / `Ctrl-Q` はTUIを終了する。workspace上のConfig overlayでは両方を消費し、
背面のworkspace終了へ伝播しない。

## workspace 画面

| surface | 入力 | 動作 |
|---|---|---|
| Switch | `↑` / `↓` | session row選択 |
| Switch | `←` / `→` | 前 / 次のproject tab |
| Switch | `Enter` / `t` | session Closeup、または選択したnew session |
| Switch | `Ctrl-A` / `Home` | new session form |
| Switch | `:` | Overview palette |
| Switch / live pane以外のCloseup | `?` | 現在のsurfaceで使えるキーボードショートカットを表示 |
| Switch | `Ctrl-X` | 選択sessionのsafe remove |
| Switch | `Ctrl-Q` | workspace離脱／TUI終了確認 |
| Switch | `Ctrl-C` | no-op |
| management surface | `Ctrl-D` | no-op。EOTはlive terminalだけに送る |
| Add workspace | `Tab` | registered / directory |
| Add workspace | `↑` / `↓` | registered row選択 |
| Add workspace | 文字 / paste / `Backspace` | filterまたはpath編集 |
| Add workspace | `Space` | 追加対象をmark |
| Add workspace | `Ctrl-X` | 開いているprojectをdetach |
| Add workspace | `Enter` / `Esc` | add / cancel |
| project / session finder | `↑` / `↓` | row選択 |
| project / session finder | 文字 / paste / `Backspace` | fuzzy filter |
| project / session finder | `1` … `9` | projectへ直接移動 |
| project / session finder | `Ctrl-X` | project rowをdetach。session rowではno-op |
| project / session finder | `Enter` / `Esc` | open / cancel |
| tabのないCloseup | `a` / `t` | Agent / Terminal |
| tabのないCloseup | `Enter` | Action modal |
| tabのあるCloseup | `Ctrl-O ?` | live paneで使えるキーボードショートカットを表示 |
| Overview palette | `↑` / `↓` | candidate / history |
| Overview palette | `←` / `→` | caret移動 |
| Overview palette | `Tab` / `Enter` / `Esc` | complete / run / close |
| Closeup Action | `↑` / `↓` | action選択 |
| Closeup Action | `←` / `→` | collapse / expand |
| Closeup Action | `Tab` / `Enter` / `Esc` | complete / run / close |
| Create session | `↑` / `↓` | branch選択 |
| Create session | `Tab` | role選択 |
| Create session | `Enter` / `Esc` | create / cancel |
| Create session error | `Enter` / `Esc` / `Ctrl-C` | dismiss |

## modal と drawer

| surface | 入力 | 動作 |
|---|---|---|
| Exit confirmation | `←` / `→` / `Tab` | choice移動 |
| Exit confirmation | `w` | Welcome |
| Exit confirmation | `q` / `Q` / `y` / `Y` | Quit |
| Exit confirmation | `n` / `N` / `Esc` | Stay |
| Exit confirmation | `Enter` | 選択choiceを決定 |
| Force remove confirmation | `←` / `→` / `Tab` | Yes / No |
| Force remove confirmation | `y` / `Y` / `Enter` | force remove |
| Force remove confirmation | `n` / `N` / `Esc` | cancel |
| Cleanup queue | `↑` / `↓` | session選択 |
| Cleanup queue | `Space` | mark |
| Cleanup queue | `a` / `A` | 全選択 / 全解除 |
| Cleanup queue | `Enter` / `Esc` | remove開始 / close |
| Pull Request | `←` / `→` | status tab |
| Pull Request | `↑` / `↓` | PR選択 |
| Pull Request | `c` | URL copy |
| Pull Request | `Ctrl-X` | 選択PRをdismiss |
| Pull Request | `Enter` / `Esc` | browserで開く / close |
| Preview | `↑` / `↓` / `Esc` | scroll / close |
| Scratchpad | paste / `Esc` | draftへ追記 / close |
| Daemon status | `Esc` | close |
| Decision list | `↑` / `↓` | decision選択 |
| Decision list | `Enter` / `Esc` | open / close |
| Decision answer | `↑` / `↓` | option選択 |
| Decision answer | `PgUp` / `PgDn` | prompt scroll |
| Decision answer | 文字 / paste / `Backspace` | freeform編集 |
| Decision answer | `Enter` / `Esc` | submit / listへ戻る |
| Director conversation | `Ctrl-O [` / `Ctrl-O ]` | conversation選択 |
| Director conversation | `Ctrl-O x` / `Ctrl-O r` | close / resume |
| Director conversation | `Ctrl-O ↑` / `Ctrl-O ↓` / `Ctrl-O End` | scroll |
| Director New | `↑` / `↓` | provider選択 |
| Director Goal Composer | 文字 / paste / `Backspace` | goal編集 |
| Director New | `Enter` / `Esc` | launch / cancel |
| Work Runs list | `↑` / `↓` | run選択 |
| Work Runs list | `←` / `→` | previous / next run |
| Work Runs list | `Enter` / `Esc` | actions / close |
| Work Runs confirm | `Enter` / `Esc` | confirm / back |
| Work Runs escalation | 矢印 / `Enter` / `Esc` | choice / confirm / back |
| Root Shell | `Ctrl-O n` | terminal tab追加 |
| Root Shell | `Ctrl-O [` / `Ctrl-O ]` | terminal tab選択 |
| Root Shell | `Ctrl-O z` / `Ctrl-O x` | 高さ切替 / terminal終了 |
| Root Shell | `Ctrl-O ↑` / `Ctrl-O ↓` / `Ctrl-O End` | scroll |
| Garden | `←` / `→` | 横pan |
| Garden | その他のキー / paste | wakeして閉じる |

前面に入力modal / drawerがないworkspaceの `?`、live paneの `Ctrl-O ?`、全画面の `Ctrl-?` / `Ctrl-/` は
同じ Keyboard help を開き、現在の最前面surfaceが受理する全キーボード操作を表示する。plain `?` はOverview /
Closeup paletteなどの文字入力中には入力文字として扱う。

## テキスト編集

| 入力 | 動作 |
|---|---|
| 文字 / paste | caret位置へ挿入、選択中は置換 |
| `←` / `→` | 1 Unicode scalar移動 |
| `Home` / `Ctrl-A` | 行頭 |
| `End` / `Ctrl-E` | 行末 |
| `Shift-←` / `Shift-→` | 1 scalarずつ選択 |
| `Shift-Home` / `Shift-End` | 行頭 / 行末まで選択 |
| `Backspace` / `Delete` | 前 / 後ろ、または選択範囲を削除 |

画面固有の `Tab` / `Enter` / `Esc` は各表を優先する。`Ctrl-X` は入力欄のcutには使わず、一覧で選択中の対象を
除く操作だけに使う。

## live terminal

leader待機中でない入力は、terminalが選択を保持している場合のcopy chordを除きPTYへ送る。

| 入力 | 動作 |
|---|---|
| `Ctrl-C` | AgentではSIGINT。generic shellではinterrupt後にretained scrollbackをclear |
| `Ctrl-D` | EOT |
| `Ctrl-Q` | byte `0x11` |
| `Ctrl-X` | byte `0x18` |
| generic shellの`Ctrl-L` | retained scrollbackと選択をclearし、同じbyteをPTYへ送る |
| macOS `Command-C` | terminal選択をcopy |
| Linux `Ctrl-Shift-C` | terminal選択をcopy |
| Windows `Ctrl-C` | 選択中はcopy、未選択はinterrupt |
| `Esc`、文字、Enter、Tab、Backspace、矢印 | PTYへ送る |
