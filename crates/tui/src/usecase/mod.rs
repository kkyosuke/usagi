//! TUI 面ローカルの usecase 層。画面グラフの遷移（splash → welcome → open /
//! new / config / home）とイベント処理の状態機械など、TUI に閉じた application
//! ロジックを置く。面をまたいで共有するロジック（セッション作成・設定解決など）は
//! usagi-core の usecase に置き、ここからは呼び出すだけにする。
//! 依存方向は presentation → usecase → domain（domain は usagi-core が持つ）。

pub mod application;
pub mod closeup;
pub mod overview;
