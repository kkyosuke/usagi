//! daemon 専用の infrastructure 層。各面が共有する接続（IPC プロトコル型・
//! `state.json` などの永続化・git）は usagi-core が持ち、ここには daemon だけが
//! 使う外部接続を置く（agent/シェルの PTY 所有＝`TerminalPool`・Unix domain socket の
//! IPC サーバ・プロセスグループ管理と単一インスタンスロック・daemon lifecycle の
//! 永続化（`daemon.json` / `sessions.json` / owner shard））。
//! 実 IO そのもの（socket accept・PTY fork・ファイル書き込み）は合成ルートが束ね、
//! この層はそれを注入で受けて純粋に振る舞う。v2 では必要になった時点で実装を追加する。

/// OS observation of a spawned child's process-start and process-group identity.
pub mod child_identity;
/// Durable cross-process generation authority: the registry document and the
/// current locator, bound to the daemon data directory.
pub mod generation_registry;
/// The daemon's concrete pseudo-terminal adapter.  Presentation surfaces only
/// ever receive terminal stream data through IPC; they do not own this IO.
pub mod pty;
/// Durable owner-generation runtime shards and the global resource allocator,
/// bound to the daemon data directory.
pub mod resource_store;
pub mod unix_transport;
