//! 各画面の view。画面（splash / welcome / open / new / config / home）ごとに
//! 1 モジュールを持ち、usecase が持つ画面状態を受け取って 1 フレーム分の描画を
//! 組み立てる。領域の分割は [`super::layouts`]、再利用する UI 部品は
//! [`super::widgets`] に委ね、view は「どの状態をどこに出すか」だけを担う。
//! 色は [`super::theme`] の意味的な役割で載せる。

pub mod closeup_modal;
pub mod config;
pub mod new;
pub mod overview_modal;
pub mod pr_modal;
pub mod welcome;
pub mod workspace;
