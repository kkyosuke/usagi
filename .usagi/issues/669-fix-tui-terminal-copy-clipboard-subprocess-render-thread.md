---
number: 669
title: fix(tui): terminal copy の clipboard subprocess を render thread から外す
status: todo
priority: medium
labels: [review, v2, tui, terminal, uiux, responsiveness, clipboard]
dependson: []
related: [553, 662, 665]
parent: 664
created_at: 2026-08-06T20:48:58.111610+00:00
updated_at: 2026-08-06T20:48:58.111610+00:00
---

## Finding（P2 responsiveness / input reflection）

live terminal の copy（drag 選択の release、および保持中選択の copy shortcut）は、OS clipboard へ**同期 subprocess で書き込み、その完了を render thread 上で `wait()` する**。deadline も worker 分離も無いため、clipboard helper が停止すると次の draw / input / scroll / quit が止まる。

発生経路:

```text
frame loop
  → intercept_live_terminal_control
  → handle_terminal_pointer (PointerKind::Up → PointerRelease::Copy)  /  copy_terminal_selection
  → Terminal::copy_text
  → PlatformClipboard::write_text
  → Command::new(pbcopy | wl-copy | xclip | xsel).spawn()
  → child.stdin.write_all(text)
  → child.wait()   ← render thread がここで無期限に待つ
```

- `src/runtime/clipboard.rs::write_with` は `spawn` → `stdin.write_all` → `wait()` を同期実行し、timeout を持たない。
- `crates/tui/src/presentation/mod.rs::handle_terminal_pointer` の `PointerRelease::Copy` と `copy_terminal_selection` はどちらも frame loop（`intercept_live_terminal_control` 経由）から `term.copy_text` を呼ぶ。
- `src/runtime/tui.rs::CrosstermTerminal::copy_text` は `PlatformClipboard::write_text` へ直結する。

`wl-copy` は selection ownership を保持するため即座に return しないことがあり、詰まった X server や壊れた helper では `wait()` が長時間戻らない。#665 が terminal daemon RPC を非同期化しても、この clipboard 経路は別系統で render thread に残る。write 量は選択テキスト分だけなので #665 / #668 のいずれの scope にも含まれない独立の同期 IO である。

## 修正方針

- clipboard write を render thread から外す。専用 worker/thread へ最新の copy request を渡し、frame loop は enqueue と結果 drain だけを行う。
- 1 request = 1 end-to-end deadline を持たせ、超過・spawn 失敗・helper 失敗はいずれも safe feedback（例 `clipboard is unavailable` / `clipboard timed out`）へ投影する。成功を先行表示しない。
- copy request は最新 1 件へ coalesce し、helper が遅い間に大量の pending child を生まない。in-flight は高々 1、child / pipe / thread は bounded にし、timeout 時は child を terminate → reap して zombie を残さない。
- 既存の copy 契約（drag release と保持選択の再 copy、空選択の拒否で clipboard を消さない、feedback 文言）を維持する。selection snapshot の immutability も変えない。

## 受入条件

- clipboard helper が停止しても、copy 操作後の次 frame / input / scroll / quit が 1 frame + scheduler 誤差以内に進む。
- copy の成否・タイムアウトが safe feedback として一度だけ表示され、未確定を成功と誤表示しない。
- helper が遅い間に copy を連打しても、spawn される child は bounded（coalesce 済み）で、timeout 後に zombie / 残留 pipe が無い。
- 空選択で clipboard を消さない、保持選択の再 copy、macOS `pbcopy` / Wayland `wl-copy` / X11 `xclip`・`xsel` の fallback 選択という既存挙動を維持する。

## 必須テスト

- fake clock + fake clipboard port で、hang / 遅延 / 失敗 / 成功の各 fixture が deadline 内に feedback へ収束し、render 相当の loop を block しないことを assert する。
- copy 連打が最新 1 件へ coalesce され、in-flight child 数が bound を超えないことを固定する。
- timeout 後に child terminate / reap が行われ、後続 copy が成功へ復帰できることを検証する。
- 空選択・保持選択再 copy・platform fallback の既存 unit test を維持する。

## 根拠箇所

- `src/runtime/clipboard.rs`: `write_with`（`spawn` + `wait` に deadline 無し）, `write_with_fallbacks`
- `crates/tui/src/presentation/mod.rs`: `handle_terminal_pointer`（`PointerRelease::Copy`）, `copy_terminal_selection`
- `src/runtime/tui.rs`: `CrosstermTerminal::copy_text`
- `crates/tui/src/usecase/application/terminal_selection.rs`: `ClipboardPort`
