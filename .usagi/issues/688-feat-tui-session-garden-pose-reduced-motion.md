---
number: 688
title: feat(tui): session garden の残りの pose・reduced motion・選択表示を仕上げる
status: done
priority: low
labels: [v2, tui, uiux]
dependson: []
related: [674, 687]
created_at: 2026-08-16T22:38:29.573939+00:00
updated_at: 2026-08-17T01:15:31.000000+00:00
---

## 背景

session garden は自動表示（無操作 5 分）と click 遷移まで実装済みで、動く pose は `Running` の 3 pose と idle の瞬きだけである。提案（`document/proposals/15-session-garden.md`）が挙げる残りの見た目をまとめて仕上げる。

## スコープ

- **lifecycle / phase 別 animation** を追加する。`Waiting` の `?` と耳のゆっくりした交互表示、`Creating` の 2 pose 出現、`Deleting` の段階的 dim。同時に上下へ動くのは `Running` だけに保ち、`Waiting` / idle の頻度は既存 mascot と同程度にする。
- **reduced motion の production 配線**。renderer は既に `reduced_motion` 引数を受け取るが production は `false` 固定である。`USAGI_REDUCE_MOTION=1` を composition root で読み、projection へ boolean として注入する。reduced motion では全 pose を静止姿勢に固定し、状態ラベルだけ更新する。
- **選択中 session の強調**。`>` marker と nameplate の `Role::Accent` 強調（現在 `GardenSession` に選択の概念が無い）。
- **`Failed` の safe failure summary**。現在は `failed` の一語だけで、`GardenSession` に failure summary の field が無い。表示するのは安全化した短い label に限り、raw error / path / provider-native ID は renderer へ渡さない。

## 受け入れ条件

- 同じ入力 snapshot / tick / size は byte-for-byte 同じ frame になる。
- animation の pose が変わらない tick では frame material も変わらず、不要な再描画を起こさない。
- reduced motion では tick が進んでも pose が変わらず、状態ラベルは更新される。
- full / reduced motion のどちらでも状態を text label・marker・顔で識別でき、色だけに依存しない。
- すべての行が端末幅以内で、CJK の session label も途中で壊れない。

## 備考

`interrupted` の pose は production には出ない（controller の `TargetPhase` が `Interrupted` を `Done` へ畳むため）。これは設計どおりで、変えるなら別途判断が要る。
