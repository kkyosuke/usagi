---
number: 175
title: fix(tui): PR ポップアップのタイトル消失（set_pr_state のストア巻き戻し）と幅不足時の非表示を修正
status: done
priority: high
labels: [fix, tui]
dependson: []
related: []
created_at: 2026-07-10T21:04:52.197675+00:00
updated_at: 2026-07-10T21:19:39.876926+00:00
---

## 症状

PR ポップアップ（PR モーダル）に PR タイトルが表示されなくなった。#705（PR ポップアップの状態変更＋repo グループ表示）以降に発生。

## 調査結果

描画系は健全（タイトル付きの `PrLink` を渡せばユニットテスト・実バイナリの PTY E2E ともタイトルを表示する）。regression は **データ経路**と**幅の扱い**の 2 点。

### 原因 1: `set_pr_state` がストアの内容で in-memory リストを上書きする（#705 で追加）

`usecase::workspace_state::set_pr_state` は `pr_link_store::get(root)` の結果だけを基に状態を書き換えて `set` し、その結果を `HomeState::set_pinned_pr_state` → `WorktreeList::set_pr_links` でサイドバー行にそのまま反映する。

- ストアのエントリに `title` が無い場合（過去の未解決分など）: ポップアップの状態トグル（`○`/`✕` クリック — #705 の新機能）を押した瞬間、**タイトル付きの in-memory リストがタイトル無しのストア内容に置き換わり、タイトルが消える**。
- ストアファイル自体が無い場合（session 再作成で `clear` 済みだが `state.json` の `pr` が残存し、`refold_pr_links` の additive merge で表示されているケース）: `get` が空を返し、**空リストがストアへ書かれ（実機に `"prs": []` のファイルが残っていた）、バッジ・ポップアップごと消える**。

### 原因 2: 幅が足りないとポップアップ全体が消える（#705 の枠拡大の副作用）

#705 で `PR_POPUP_INNER` が 56 → 72 に拡大（枠込み最大 76 桁）。`pr_popup_placement` は `block_w > width` のとき `None` を返すため、**端末幅が箱より狭いとタイトルどころかポップアップ自体が描画されない**。

## 修正方針

1. `set_pr_state` に現在の in-memory リストを渡し、**ストア ∪ in-memory を `PrLink::aggregate` で合成してから**状態を適用・永続化・返却する（縮小しない）。タイトル・行が消えず、未タイトルのストアには逆にタイトルが補完される。
2. ポップアップの内側幅を端末幅に合わせて縮める（`PR_POPUP_INNER` と「端末幅 − 枠 4 桁」の小さい方）。長いタイトルは既存の `clip_to_width`（全角/半角の表示幅対応）で省略記号 truncate し、幅が許す限りタイトルを表示する。repo グループ・状態グリフ・末尾アクションの表示は維持。

## テスト・確認

- `set_pr_state`: ストア無し／ストア未タイトル × in-memory タイトル有りで、返却リストがタイトル・行を保持し、ストアへ union が永続化されること。
- ポップアップ: 狭い端末幅で箱が幅内に縮み、タイトルが省略記号付きで表示されること（全角混じり含む）。
- `cargo fmt` / `clippy --all-targets -- -D warnings` / `cargo test`、カバレッジ 100% 維持。
