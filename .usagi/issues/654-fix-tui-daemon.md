---
number: 654
title: fix: TUI/daemon 安定性レビューの指摘を解消する
status: todo
priority: high
labels: [review, stability, tui, daemon, epic]
dependson: []
related: [221]
created_at: 2026-08-05T01:14:16.788040+00:00
updated_at: 2026-08-05T01:21:35.758343+00:00
---

## 背景

2026-08-05 の `origin/main`（レビュー開始時 `3e21b392`、起票前に最新 `3436fd5d` へ再同期）を、TUI の表示・入力・scrollback と daemon の接続・再接続・障害復旧の観点でレビューした。

request deadline、owner/workspace/generation fence、terminal input の ACK 不確実性、stale endpoint recovery、planned seamless rollover、raw mode teardown などの基礎契約は十分に堅い。一方で、production input adapter の経路差、診断 observer の自己劣化、daemon background worker の部分故障、危険な運用案内のドリフト、editor/scrollback の操作不足が残っている。

本 issue は、安定利用を阻む指摘を独立した子 issue に分割して追跡する親 issue である。

## 子 issue と依存順

| issue | priority | 内容 | dependson |
|---|---|---|---|
| #655 | high | production input adapter を統一し chord・paste・PTY key を欠落させない | — |
| #656 | high | 未消費 metrics subscription による health warning の自己誘発を止める | — |
| #657 | high | critical background worker の停止を監視し部分故障を残さない | — |
| #658 | high | restart の seamless rollover と `--force` の破壊性を案内へ反映する | — |
| #659 | medium | doctor と daemon status を実動診断・安全な復旧案内へ接続する | #657 |
| #660 | medium | `roles.toml` editor を cursor・selection・scroll 対応にする | #655 |
| #661 | medium | Notes overlay を production 導線・描画・編集入力へ接続する | #655 |
| #662 | medium | scrollback 閲覧位置を固定し page 移動・未読表示を追加する | #655 |

着手順は、独立した P1（#655–#658）を先に進め、production input の正本を確定した後に #660–#662、worker health の権威を確定した後に #659 とする。

## 優先順位

### P1: 安定利用前に直す

- 実端末入力を単一経路へ統合し、管理 chord・paste・PTY key bytes を失わない。
- 未消費 metrics subscription が `dropped_updates` と health warning を自己誘発しない。
- daemon の critical background worker が停止しても PID だけ生きた部分故障を残さない。
- `daemon restart` の seamless rollover と `--force` の破壊性を README / overview / CLI 案内で一致させる。

### P2: 運用・編集 UX を安定させる

- `daemon status` / `doctor` を IPC readiness・worker health・安全な復旧案内へ接続する。
- `roles.toml` editor を末尾 append/pop だけでなく、既存 source を損失なく修正できる editor にする。
- Notes overlay の production 導線と編集入力を完成させるか、到達不能な surface を整理する。
- scrollback 閲覧中の絶対位置を新規出力から守り、page 単位の移動と live-bottom 復帰を提供する。

## 共通受け入れ条件

- production composition を通る crossterm / Unix socket / real PTY の回帰テストを持つ。reducer へ最終語彙を直接注入する unit test だけで成立扱いにしない。
- `cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets -- -D warnings`、risk-based selected tests、PR CI の full test / coverage 100% を通す。
- ユーザー入力、PTY output、secret、raw protocol detail を health/status/logへ新たに載せない。
- 実装変更に合わせて `document/03-tui.md` / `document/05-daemon.md` / `README.md` の正本を更新する。

## 非目標

- daemon crash / SIGKILL / OS reboot 後の PTY master FD 継続。これは #221 と `document/proposals/07-pty-crash-continuation.md` の採否条件・将来分割を正本とし、安定化修正へ暗黙に混ぜない。
- local PTY fallback、PID だけからの ownership 推測、証明できない stale artifact の自動削除。
