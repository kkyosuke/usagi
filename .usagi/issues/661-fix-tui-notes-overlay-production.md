---
number: 661
title: fix(tui): Notes overlay を production 導線・描画・編集入力へ接続する
status: todo
priority: medium
labels: [review, stability, tui, notes, input, ux]
dependson: [655]
related: [155, 157, 162, 170]
parent: 654
created_at: 2026-08-05T01:20:37.970677+00:00
updated_at: 2026-08-05T01:20:37.970677+00:00
---

## 症状

v2にはNotes overlay用の`AppKey`、`NoteEditor`、load/save effect、backend port、`render_notes_over`が存在する。しかしproduction Homeでは次が欠けており、利用者から到達・操作できない。

- `OpenNotes`を生成するkeyboard/command/click routeがない。
- `HomeFrameMaterial`はEnvironment/Role editorを合成するが、`note_editor`をmaterialへ載せず`render_notes_over`を呼ばない。
- overlay中の通常`Char`/Backspace/Tab/Enter/Ctrl-S等を`SetNoteDraft`、section移動、commit/toggle/saveへ写すproduction input routeがない。

sidebarのnote iconは内容の有無だけを示す表示専用であり、現状のstate/effect/renderer testはproduction到達性を証明しない。

## 既存issueとの境界

#155/#157/#162/#170はscratchpadと旧TUIのnote/todos/decisions UXを実装済みとしている。本issueは現行v2 controller loopへ残ったlast-mile断を対象とし、MCP/store schemaを再実装しない。#655は共通input adapterとpaste/Ctrl-Sを担当する。

## 修正方針

- Notesを開く一つの明示的なproduction routeをcommand registryまたはdocumented chord/clickへ追加する。表示専用note iconをclickableにする場合はrender/hit-testを同じlayout projectionから導く。
- `HomeFrameMaterial`へ`NoteEditor`を含め、overlay precedenceに従って`render_notes_over`を合成する。
- section、draft、todo selection、save中/errorを持つinput reducerを実端末`Key`から接続する。
- backend load/save failureでもdraftと編集snapshotを失わずretry可能にする。

## 受け入れ条件

- Switch/Closeupのdocumented routeからactive targetのNotesを開け、root/session targetをstable identityでloadする。
- overlayがHome背景の上に実際に描画される。
- noteの自由編集、todoの選択/追加/編集/完了toggle、decisionsのread-only表示が明示された操作表どおり動く。既存の簡略UXを採る場合も、表示footerと実入力を一致させる。
- Ctrl-S save、Esc close、paste、cursor/selectionはproduction adapterを通り、背後live PTYやsidebarへ漏れない。
- save/load failure後もdraftを保持し、安全なerrorだけを表示する。
- workspaceを離れた後のlate completionが別workspace/targetのeditorを更新しない。
- fake crossterm/real frame loopでopen→load→edit→save→closeを通すintegration testを追加する。

## 不採用時の代替

製品としてNotes overlayを提供しない判断をする場合は、到達不能な`Overlay::Notes`、`AppKey`、renderer/effect/portを削除し、README/docsから機能主張を外す。dead APIを残して完了扱いにしない。

## docs

`document/03-tui.md` に現在提供するNotes導線と操作だけを記載する。
