//! 画面をまたいで再利用する UI 部品（widget）。v1 の `widgets/` を引き継ぎ、
//! `picker` / `dir_picker` / `text_input`（キャレット編集付き 1 行入力）/
//! `text_area`（複数行入力）/ `rabbit`（うさぎ AA）などを置く。
//! 特定の画面に固有の描画は [`super::views`] に置き、ここには複数 view から
//! 使い回す部品だけを置く。
