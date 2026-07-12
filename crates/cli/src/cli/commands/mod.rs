//! 人間向け CLI サブコマンドのハンドラ置き場。**1 コマンド = 1 ファイル**とし、
//! 各ファイルのハンドラ型が `Run` を実装する。`cli/mod.rs` の dispatch
//! （`Command::into_handler`）が解釈済みコマンドを対応ハンドラに変換し、実行は
//! `Run::run` の一様な呼び出しになる。
//!
//! 各ハンドラは presentation に徹する — 解析済みのオプションを保持し、TUI/daemon 面への
//! 委譲や core usecase 呼び出し・結果整形を行う（独自のビジネスロジックは持たない）。
//!
//! TUI を開くハンドラは起動要求を返し、`update` / `version` / `completion` は結果を出力する。
//! エージェント統合フック（Claude が呼ぶ `guard-workspace` / `agent-phase`）は人間向けでは
//! ないため、ここではなく [`crate::cli::hooks`] に置く。

pub mod completion;
pub mod config;
pub mod doctor;
pub mod hop;
pub mod open;
pub mod update;
pub mod version;

pub use completion::Completion;
pub use config::Config;
pub use doctor::Doctor;
pub use hop::Hop;
pub use open::Open;
pub use update::Update;
pub use version::Version;
