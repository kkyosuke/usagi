// Persistence, Git operations, and other external integrations live here.
pub mod agent;
pub mod agent_prompt_store;
pub mod agent_state_store;
pub mod error_log;
pub mod git;
pub mod gitignore;
pub mod history_store;
pub mod issue_store;
pub mod json_file;
pub mod memory_store;
pub mod pty;
pub mod release;
pub mod session_monitor;
pub mod storage;
pub mod terminal;
pub mod workspace_store;
