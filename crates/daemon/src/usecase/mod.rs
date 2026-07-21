//! daemon 専用の usecase 層。TUI と共有するロジック（セッション作成・設定解決など）は
//! usagi-core の usecase に置き、ここには daemon だけが駆動するロジックを置く
//! （daemon 起動・セッション監視ティック・委譲 queue の消化＝autostart・
//! waiting/done 通知の調停・孤児端末 adopt の判定）。domain は usagi-core を再利用し、
//! 実 IO は注入する。v2 では必要になった時点で実装を追加する。
//!
//! 制御プレーンの verb はすべて実 IO（store 読取・生存判定・signal・常駐待受・プロセス
//! 起動）を要するため、各 usecase モジュール（[`serve`] / [`start`] / [`status`] / [`stop`] /
//! [`restart`]）が担い、[`crate::presentation::run`] が検証済みの
//! [`crate::presentation::DaemonCommand`] を振り分ける。argv の解釈と不正な command の
//! 拒否は合成ルートが担う。

pub mod agent_ipc;
pub mod claude;
pub mod codex;
pub mod control;
pub mod generation;
pub mod generic_terminal;
pub mod metrics;
pub mod orchestration;
pub mod pr_inventory;
pub mod restart;
pub mod runtime;
pub mod serve;
pub mod session_runtime;
pub mod start;
pub mod status;
pub mod stop;
pub mod supervisor_runtime;
pub mod terminal;
pub mod terminal_ipc;
pub mod terminal_profile;
