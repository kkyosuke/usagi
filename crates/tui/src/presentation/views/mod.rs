//! 各画面の view。画面（splash / welcome / open / new / config / home）ごとに
//! 1 モジュールを持ち、usecase が持つ画面状態を受け取って 1 フレーム分の描画を
//! 組み立てる。領域の分割は [`super::layouts`]、再利用する UI 部品は
//! [`super::widgets`] に委ね、view は「どの状態をどこに出すか」だけを担う。
//! 色は [`super::theme`] の意味的な役割で載せる。

pub mod closeup_modal;
pub mod config;
pub mod create_session_modal;
pub mod decision_modal;
pub mod new;
pub mod open;
pub mod overview_modal;
pub mod pr_modal;
pub mod quit_modal;
pub mod remove_modal;
pub mod scratchpad_modal;
pub mod splash;
pub mod text_overlay;
pub mod welcome;
pub mod workspace;
