---
number: 408
title: fix(tui): Ctrl-O Ctrl-X で active tab を閉じる
status: done
priority: high
labels: [tui, closeup, input, pane, bug]
dependson: []
related: [224, 278, 303]
created_at: 2026-07-20T05:18:54.677221+00:00
updated_at: 2026-07-20T05:30:58.258313+00:00
---

## 目的

Closeup の live terminal / Agent pane で、close chord を `Ctrl-O` leader に続く `Ctrl-X` として受理し、active tab を確実に閉じる。現状は plain `x` だけを `CloseTab` に分類し、`Ctrl-X` は unknown follow-up として swallow するため、期待する chord で tab が閉じない。

## 調査根拠

- `LiveInputClassifier::prefix_action` は `Ctrl-O` 後の control chord として `Ctrl-O` / `Ctrl-A` / `Ctrl-N` / `Ctrl-P` を扱う一方、close は modifier 無しの `x` に限定している。
- runtime は解決済み `LiveTerminalAction::CloseTab` を `intercept_live_terminal_control` で受け、`close_focused_terminal_pane` により live subscription の detach または pending launch の cancel を行う。したがって close 実行経路ではなく classifier 契約の不一致が原因である。
- v2 正本は close chord を `Ctrl-O x` とし、Closeup footer は close chord 自体を表示していないため、期待する `Ctrl-O Ctrl-X` が UI からも分からない。

## スコープ

- `LiveInputClassifier` が semantic `Ctrl-X` と control byte U+0018 の双方を leader follow-up の `CloseTab` として分類する。
- plain `x` の既存互換を維持する。
- leader 無しの `Ctrl-X` は従来どおり PTY passthrough とし、TUI の global shortcut にしない。
- classifier と runtime close dispatch の回帰テストを追加・更新する。
- Closeup footer と `document/03-tui.md` の close chord 表記を実装に合わせる。

## 対象外

- daemon / IPC の terminal close protocol の変更。
- tab close 後の選択規則や pending cancel semantics の変更。
- Ctrl-O prefix の timeout、unknown follow-up、その他 chord の変更。

## 完了条件

- active live tab で `Ctrl-O Ctrl-X` を入力すると tab が閉じ、live subscription が detach される。
- active pending tab でも同 chord が placeholder と queued launch を取消す。
- semantic modifier event と U+0018 control byte の両入力形式で `CloseTab` に解決する。
- plain `Ctrl-X` は pane へ一度だけ passthrough され、plain `Ctrl-O x` の互換も維持される。
- footer・v2 TUI 正本・回帰テストが同じ PR に含まれる。
