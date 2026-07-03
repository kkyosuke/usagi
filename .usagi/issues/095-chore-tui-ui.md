---
number: 95
title: chore(tui): UI 文言ポリシーと用語・アセットの整理
status: todo
priority: low
labels: [tui, review]
dependson: []
related: []
created_at: 2026-07-03T23:22:05.112860+00:00
updated_at: 2026-07-03T23:22:05.112860+00:00
---

UI/UX レビュー（2026-07 branch `usagi/ui`）由来。低優先の一貫性・磨き込み。

## 項目
1. **日英混在にポリシーがない**: 同一 Config 画面でフッターは英語・LLM モーダルは日本語（`config/ui.rs`）、確認モーダル同士でも言語が割れる（終了/削除は英語、アップデート/env は日本語）。gallery フッター「なにかキーで終了」等。→「システム操作＝英語、マスコット・通知＝日本語」等の基準を design/README.md に明記し既存文言を寄せる。
2. **project / workspace の用語ゆれ**: `open/ui.rs` は同一画面で `Open Project` / `Choose one registered workspace` / `No workspaces yet.` / `No projects match the filter.` と呼び分け。New（"New Project" で workspace 登録）や Config の "Default Workspace" とも混在。→ ユーザー向け文言を片方に統一。
3. **welcome の Esc=終了が事故導線**（`welcome/menu.rs`）: サブ画面から Esc で戻った直後の反射的な Esc がアプリを終了させる。→ 要検討（no-op か Quit 項目へのフォーカス移動、終了は `q`/`Ctrl+C` に限定）。
4. **Nerd Font 要件の明記/フォールバック**: PUA グリフ多用なのに README にフォント要件がなく、非対応で豆腐＋列ずれ。PR バッジ（`ea64` codicon）は「FA4 範囲を選ぶ」方針から外れている。→ README/doctor に要件明記、ASCII フォールバックか FA4 範囲へ寄せる。
5. **splash / 滑空アニメの設定連動・スキップ**: `Mascot Animation: Off` がサイドバーのうさぎしか止めず、splash（約 1.5 秒）・open→home 滑空（約 1.3 秒）に効かない。→ 設定 Off で省略/短縮。あわせて splash/gallery の stale rustdoc（「runs back and forth」→ 実装は静止＋タイトルフェード）と `state/mode.rs` の旧キー名コメントを修正。
6. **パレットのキーヒント二重表示**（`home/ui/chrome.rs`）: ボックス内とフッターで同じ情報を並び順違いで 2 か所表示。→ 一本化または並び順統一。

## 受け入れ条件
- UI 文言の言語/用語が方針に沿って一貫。フォント要件が明記。アニメが設定連動。
- カバレッジ 100% 維持。
