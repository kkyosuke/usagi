---
number: 680
title: fix(tui): clipboard helper の wait を deadline と cleanup で bounded にする
status: todo
priority: medium
labels: [review, v2, tui, clipboard, process, freeze, resource]
dependson: []
related: [307, 390, 662]
parent: 671
created_at: 2026-08-13T22:45:35.558571+00:00
updated_at: 2026-08-13T22:45:35.558571+00:00
---

## Finding（P2 UI freeze / process lifecycle）

`src/runtime/clipboard.rs::write_with` はcopy操作を処理するTUI thread上で`pbcopy` / `clip.exe` / `wl-copy` / `xclip` / `xsel`をspawnし、stdinへ全textを書いた後に`child.wait()`する。timeout、cancellation、process-group cleanupがない。

clipboard backendがhang、stdinを読まない、descendantを残す、またはdesktop service待ちになると、mouse release / copy shortcutからTUIの入力・描画が無期限停止する。#662 の `PlatformChildReaper` はnotification/browser/external-terminalのzombie回収だけで、clipboardは同期waitの別経路である。

## 修正方針

- clipboard writeをrender/input threadからbounded workerへ逃がすか、deadline付きowned child primitiveで完了させる。
- stdin writerとchild/process groupのlifecycleを同じownerが持ち、timeout時にclose → TERM → bounded grace → KILL → reapする。
- selection textのbyte capはterminal selection/scrollback契約から導き、worker queueもhard bound/coalesceする。
- timeout/overflow/backend failureはsafe feedbackにし、TUIと既存clipboard内容を成功扱いで上書きしない。
- fallbackは各backendのbounded failure後だけ次へ進み、1 backendが後続を永久に塞がない。

## 受入条件

- [ ] stdinを読まない/hangするfixtureでもTUI copy callまたはcompletionがbounded time内に戻る。
- [ ] parent/descendant、normal success、nonzero、broken pipeの全経路でchildをreapしzombie/pipeを残さない。
- [ ] copy burstでthread/process/queueを無制限生成せず、latest selectionまたは明示refusalへ収束する。
- [ ] macOS / Windows / Wayland / X11のfallback順とsafe feedbackを維持する。

## 根拠箇所

- `src/runtime/clipboard.rs`
- `src/runtime/tui.rs::CrosstermTerminal::copy_text`
- `crates/tui/src/presentation/mod.rs` のpointer/copy handler
