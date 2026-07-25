//! MCP (Model Context Protocol) servers and their shared JSON-RPC plumbing.
//!
//! usagi speaks MCP over stdio so AI agents (Claude Code etc.) can drive it with
//! the same operations a human uses on the CLI. The servers here:
//!
//! - [`usagi`] is the single server launched by `usagi mcp`. It composes the
//!   issue/memory and session servers below so one process exposes a
//!   repository's task issues, its durable memories, and session orchestration
//!   under one `usagi` registration.
//! - [`issue`] exposes a repository's task issues.
//! - [`memory`] exposes a repository's durable memories.
//! - [`session`] exposes session orchestration (create / list / prompt) as tools.
//! - [`llm`] exposes a locally-running model as a single delegation tool.
//!
//! All speak JSON-RPC 2.0 with newline-delimited messages and implement the
//! small subset MCP needs (`initialize`, `tools/list`, `tools/call`, `ping`)
//! directly over `serde_json` — no async runtime, so each request is handled by
//! a plain synchronous, unit-testable function. The framing (parsing, method
//! dispatch, response shaping) is identical between them and lives here; each
//! server only supplies the parts that differ via [`McpService`].
//!
//! [`serve`] reads requests sequentially but runs them on a small bounded thread
//! pool, so one slow tool call (`session_remove` on a large worktree takes
//! minutes) does not stall every following request on the same connection. See
//! [`serve`] for the concurrency model.

pub mod issue;
pub mod llm;
pub mod memory;
pub mod session;
pub mod usagi;

use std::backtrace::Backtrace;
use std::io::{BufRead, Read, Write};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Mutex, MutexGuard};

use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Value};

/// MCP protocol version these servers implement.
pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// Upper bound on the bytes [`serve`] buffers for a single request line before
/// rejecting it. `read_until` grows its buffer until it sees a newline or EOF, so
/// without a cap one newline-less line from a wedged or hostile stdio peer would
/// grow memory without bound (OOM). 64 MiB is far above any real JSON-RPC request
/// usagi receives while still bounding the damage.
const MAX_REQUEST_LINE_BYTES: u64 = 64 * 1024 * 1024;

/// Upper bound on the JSON argument dump attached to an MCP panic log entry.
/// Arguments are local diagnostics, not client-facing output, but prompts can be
/// large; keep one panic from producing an unbounded error-log entry.
const MAX_PANIC_ARGUMENT_CHARS: usize = 8 * 1024;

/// How many requests [`serve`] runs at once. Requests are mostly IO-bound (git,
/// the filesystem, an agent CLI), and an agent drives one connection with a
/// handful of tools in flight, so a small pool covers real use while keeping the
/// thread count and the number of concurrent store mutations predictable.
const DISPATCH_WORKERS: usize = 8;

/// How many further requests [`serve`] admits while every worker is busy. Bounds
/// the queue so a client that keeps sending while a slow tool runs is refused
/// explicitly instead of making the server buffer without bound.
const DISPATCH_QUEUE: usize = 64;

/// JSON-RPC error code returned when the bounded dispatch pool is saturated.
/// `-32000` is inside the implementation-defined server-error range reserved by
/// JSON-RPC 2.0 (`-32000..=-32099`); it is not a protocol violation by the
/// client, so it must not reuse one of the predefined codes.
const SERVER_BUSY_CODE: i64 = -32000;

/// The outcome of reading one capped request line (see [`read_capped_line`]).
enum LineRead {
    /// End of input: no more requests.
    Eof,
    /// A complete line is in the buffer (terminating newline included).
    Line,
    /// The line exceeded the cap; its remainder was drained so the next read
    /// resyncs on a real boundary. No usable line is in the buffer.
    TooLong,
}

/// Read one newline-terminated line from `input` into `raw`, buffering at most
/// `max` bytes. A line longer than `max` is reported as [`LineRead::TooLong`] and
/// its remainder drained in bounded chunks, so a never-terminating line can never
/// grow the buffer without bound. `raw` is cleared on entry.
fn read_capped_line<R: BufRead>(
    input: &mut R,
    raw: &mut Vec<u8>,
    max: u64,
) -> std::io::Result<LineRead> {
    raw.clear();
    let read = input.by_ref().take(max).read_until(b'\n', raw)?;
    if read == 0 {
        return Ok(LineRead::Eof);
    }
    // The cap was reached without consuming the line's terminating newline: the
    // line is longer than we will buffer. Drain the rest in bounded chunks so the
    // following read starts at the next real line, and report it too-long.
    if read as u64 == max && !raw.ends_with(b"\n") {
        let mut discard = Vec::new();
        loop {
            discard.clear();
            let n = input.by_ref().take(max).read_until(b'\n', &mut discard)?;
            if n == 0 || discard.ends_with(b"\n") {
                break;
            }
        }
        return Ok(LineRead::TooLong);
    }
    Ok(LineRead::Line)
}

/// Deserialize tool arguments into `T`, mapping any error to a tool-facing
/// message. Shared by every MCP server's tool handlers.
pub(crate) fn parse_args<T: DeserializeOwned>(arguments: Value) -> Result<T, String> {
    serde_json::from_value(arguments).map_err(|e| format!("invalid arguments: {e}"))
}

/// Pretty-print a serialisable tool result as JSON, falling back to an empty
/// string on the (practically unreachable) serialisation error. Shared by every
/// MCP server's tool handlers.
pub(crate) fn to_pretty<T: Serialize + ?Sized>(value: &T) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

/// Unwrap a tool-schema [`Value`] into its array of entries, used by composite
/// servers to merge the schemas of the servers they wrap.
///
/// The schema builders return JSON arrays by construction, so this normally just
/// takes the inner `Vec`. A non-array (a construction bug) degrades to no
/// entries rather than panicking: `tools/list` is on the hot path and a panic
/// there would abort the whole stdio server — taking every tool down — instead
/// of merely advertising fewer tools.
pub(crate) fn into_schema_array(value: Value) -> Vec<Value> {
    match value {
        Value::Array(items) => items,
        _ => Vec::new(),
    }
}

/// Tunables for [`serve`]'s read loop and its bounded dispatch pool, so tests can
/// drive the too-long-line and saturation paths with small budgets instead of a
/// 64 MiB input and 72 concurrent requests.
struct ServeLimits {
    /// Bytes buffered for a single request line before refusing it.
    max_line_bytes: u64,
    /// Requests executed concurrently.
    workers: usize,
    /// Requests admitted beyond `workers` while every worker is busy.
    queue: usize,
}

impl Default for ServeLimits {
    fn default() -> Self {
        Self {
            max_line_bytes: MAX_REQUEST_LINE_BYTES,
            workers: DISPATCH_WORKERS,
            queue: DISPATCH_QUEUE,
        }
    }
}

/// Run the MCP loop for `service` over the given streams: read newline-delimited
/// JSON-RPC requests, skip blank lines, and write each reply back, flushing per
/// line. Generic over its streams so it is driven by stdio in production and by
/// in-memory buffers in tests.
///
/// # Concurrency model
///
/// Reading is sequential, but a request that expects a reply is handed to a
/// bounded pool of [`DISPATCH_WORKERS`] threads instead of being run on the
/// reading thread. A tool call that takes minutes (`session_remove` on a large
/// worktree) therefore no longer stalls every following request on the same
/// connection.
///
/// | Aspect | Behaviour |
/// |---|---|
/// | Concurrency | Up to [`DISPATCH_WORKERS`] tool calls run at once |
/// | Back pressure | [`DISPATCH_QUEUE`] further requests are admitted; beyond that a request is refused with [`SERVER_BUSY_CODE`] |
/// | Reply order | Not the request order — replies come out as each request finishes, correlated by JSON-RPC `id` |
/// | Reply framing | One reply per line: writes are serialised, so lines never interleave |
/// | Notifications | Have no `id` and take no reply, so they never occupy a worker or a queue slot |
/// | Parse-level replies | `-32700` / `-32600` need no tool, so the reading thread writes them directly |
/// | Shutdown | On EOF (or a write error) the loop stops reading and waits for in-flight requests to finish before returning |
///
/// Concurrent tool calls do not add data races: mutations of the shared markdown
/// stores already serialise behind the cross-process advisory lock in
/// [`crate::infrastructure::store_lock`], which is taken per open file and so
/// serialises threads of one process just as it does separate processes.
pub fn serve(
    service: &(dyn McpService + Sync),
    input: impl BufRead,
    output: impl Write + Send,
) -> std::io::Result<()> {
    serve_with_limits(service, input, output, &ServeLimits::default())
}

/// [`serve`] with explicit [`ServeLimits`].
fn serve_with_limits(
    service: &(dyn McpService + Sync),
    mut input: impl BufRead,
    output: impl Write + Send,
    limits: &ServeLimits,
) -> std::io::Result<()> {
    let writer = ResponseWriter::new(output);
    let admission = Admission::new(limits.workers + limits.queue);
    let (sender, receiver) = mpsc::channel::<PendingRequest>();
    let receiver = Mutex::new(receiver);

    let read_result = std::thread::scope(|scope| {
        for _ in 0..limits.workers {
            scope.spawn(|| dispatch_worker(service, &receiver, &writer, &admission));
        }
        let result = read_requests(
            &mut input,
            &writer,
            &admission,
            &sender,
            limits.max_line_bytes,
        );
        // Moves `sender` into this closure so it is gone before the scope joins
        // the workers: that disconnect is what tells them no more requests are
        // coming once the queue drains.
        drop(sender);
        result
    });

    // A read error (e.g. a broken pipe on stdin) is the primary failure; a write
    // error recorded by a worker is reported when reading itself ended cleanly.
    read_result?;
    match writer.into_error() {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// Read request lines from `input` until EOF, replying directly to the ones that
/// need no tool and handing the rest to the dispatch pool through `sender`.
fn read_requests<W: Write>(
    input: &mut impl BufRead,
    writer: &ResponseWriter<W>,
    admission: &Admission,
    sender: &Sender<PendingRequest>,
    max_line_bytes: u64,
) -> std::io::Result<()> {
    // Read raw bytes and decode lossily rather than using `BufRead::lines`, which
    // yields an `Err` on a line containing invalid UTF-8 — propagating that would
    // let one malformed byte sequence from a misbehaving client terminate the
    // whole server. A non-UTF-8 line instead becomes replacement characters that
    // fail to parse as JSON, so [`classify_line`] yields a `-32700 parse error`
    // and the loop keeps going. A genuine IO error (e.g. a broken pipe) still
    // propagates and ends the loop.
    let mut raw = Vec::new();
    loop {
        // Once a write has failed every further reply is undeliverable, so stop
        // reading rather than running more tools for a client that is gone.
        if writer.failed() {
            break;
        }
        match read_capped_line(input, &mut raw, max_line_bytes)? {
            LineRead::Eof => break,
            // A pathologically long line (a wedged/hostile producer) is refused
            // with a parse error rather than buffered without bound; the loop
            // keeps serving the next request.
            LineRead::TooLong => {
                writer.write_line(&error_response(
                    Value::Null,
                    -32700,
                    "parse error: request too large",
                ));
                continue;
            }
            LineRead::Line => {}
        }
        let line = String::from_utf8_lossy(&raw);
        let line = line.trim_end_matches(['\n', '\r']);
        if line.trim().is_empty() {
            continue;
        }
        match classify_line(line) {
            // A notification carries no id and takes no reply, so it neither
            // occupies a worker nor can be refused for lack of one.
            Incoming::Ignored => {}
            Incoming::Immediate(response) => writer.write_line(&response),
            Incoming::Request(request) => {
                // `initialize` / `ping` / `tools/list` go through the pool too:
                // a client waits for `initialize` before sending anything else,
                // so nothing depends on them bypassing it, and one code path is
                // easier to reason about than a fast lane beside it.
                dispatch_or_refuse(request, writer, admission, sender);
            }
        }
    }
    Ok(())
}

/// Queue `request` for the dispatch pool, or refuse it when the pool's bounded
/// capacity is already taken by requests in flight.
fn dispatch_or_refuse<W: Write>(
    request: PendingRequest,
    writer: &ResponseWriter<W>,
    admission: &Admission,
    sender: &Sender<PendingRequest>,
) {
    if !admission.try_admit() {
        // Refuse loudly. Silently queueing would reproduce the very stall this
        // pool exists to remove, only with an unbounded memory cost.
        writer.write_line(&error_response(
            request.id,
            SERVER_BUSY_CODE,
            &format!(
                "server busy: {} requests already in flight; retry this request",
                admission.capacity
            ),
        ));
        return;
    }
    // The receiver is owned by `serve_with_limits`, whose scope outlives this
    // loop, so the channel cannot be disconnected here. Assert that rather than
    // dropping the request, which would leave the client waiting forever.
    sender
        .send(request)
        .expect("mcp dispatch queue receiver outlives the read loop");
}

/// One dispatch-pool thread: take the next request, run it, write its reply.
fn dispatch_worker<W: Write>(
    service: &dyn McpService,
    receiver: &Mutex<Receiver<PendingRequest>>,
    writer: &ResponseWriter<W>,
    admission: &Admission,
) {
    loop {
        // Hold the receiver lock only while taking the next request: exactly one
        // worker waits on the channel and the others wait on the mutex, so a
        // long-running tool never blocks the hand-off of the next request.
        let taken = {
            let queue = receiver.lock().expect("mcp dispatch queue lock");
            queue.recv()
        };
        // The read loop dropped its sender and the queue is drained: shut down.
        let Ok(request) = taken else { break };
        writer.write_line(&run_request(service, request));
        admission.release();
    }
}

/// Bounds how many requests may be in flight — queued or running — at once.
///
/// A slot is taken when a request is admitted and released once its reply has
/// been written, so admission never depends on whether a worker has already
/// picked the request up. That makes the refusal boundary a property of the
/// server's load rather than of thread scheduling.
struct Admission {
    in_flight: AtomicUsize,
    capacity: usize,
}

impl Admission {
    fn new(capacity: usize) -> Self {
        Self {
            in_flight: AtomicUsize::new(0),
            capacity,
        }
    }

    /// Take a slot, or report that the server is saturated.
    fn try_admit(&self) -> bool {
        // Compare-and-swap rather than a plain `fetch_add` + undo: the count must
        // never transiently exceed the capacity, or two threads racing at the
        // boundary could each see room that only one of them has.
        let mut in_flight = self.in_flight.load(Ordering::SeqCst);
        loop {
            if in_flight >= self.capacity {
                return false;
            }
            match self.in_flight.compare_exchange_weak(
                in_flight,
                in_flight + 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return true,
                // Another thread moved the count first; retry against its value.
                Err(current) => in_flight = current,
            }
        }
    }

    /// Give back a slot taken by [`try_admit`].
    fn release(&self) {
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Serialises replies onto the output stream so concurrently-dispatched requests
/// each produce one whole line, never interleaved fragments.
///
/// The first write error is remembered instead of being propagated out of
/// whichever worker hit it, so the read loop can stop and [`serve`] can return
/// it from the one place that owns the stream.
struct ResponseWriter<W> {
    state: Mutex<WriterState<W>>,
}

struct WriterState<W> {
    output: W,
    error: Option<std::io::Error>,
}

impl<W: Write> ResponseWriter<W> {
    fn new(output: W) -> Self {
        Self {
            state: Mutex::new(WriterState {
                output,
                error: None,
            }),
        }
    }

    /// Write one reply as a single line and flush it. Once a write has failed
    /// every further reply is dropped: the stream is gone, and [`serve`] is
    /// already on its way to returning that error.
    fn write_line(&self, response: &str) {
        let mut state = self.lock();
        if state.error.is_some() {
            return;
        }
        let outcome = writeln!(state.output, "{response}").and_then(|()| state.output.flush());
        if let Err(error) = outcome {
            state.error = Some(error);
        }
    }

    /// Whether a write has failed, so the read loop can stop early.
    fn failed(&self) -> bool {
        self.lock().error.is_some()
    }

    fn lock(&self) -> MutexGuard<'_, WriterState<W>> {
        self.state.lock().expect("mcp response writer lock")
    }

    /// The first write error, once every worker has finished writing.
    fn into_error(self) -> Option<std::io::Error> {
        self.state
            .into_inner()
            .expect("mcp response writer lock")
            .error
    }
}

/// The per-server behaviour an MCP server must supply. The JSON-RPC framing is
/// handled once by [`dispatch_line`]; implementors only describe their identity
/// and tools.
pub trait McpService {
    /// `serverInfo.name` advertised during `initialize`.
    fn server_name(&self) -> &str;

    /// Tool names this service handles. Composite servers use this to route a
    /// call to the first sub-server that advertises the requested tool.
    fn tool_names(&self) -> &'static [&'static str] {
        &[]
    }

    /// Tool schemas advertised via `tools/list`.
    fn tool_schemas(&self) -> Value;

    /// Run a tool by name, returning its text payload (`Ok`) or an error
    /// message to surface to the agent (`Err`).
    fn call_tool(&self, name: &str, arguments: Value) -> Result<String, String>;
}

/// A JSON-RPC request that expects a reply, carried from the reading thread to a
/// dispatch worker.
#[derive(Debug)]
struct PendingRequest {
    method: String,
    params: Option<Value>,
    id: Value,
}

/// What one line of input turns into, decided by [`classify_line`] before any
/// tool runs so [`serve`] knows whether it must occupy a dispatch worker.
enum Incoming {
    /// A notification: acted on without a reply, so there is nothing to send and
    /// nothing to dispatch.
    Ignored,
    /// A reply determined by framing alone (parse error, malformed request). It
    /// needs no tool, so the reading thread writes it directly.
    Immediate(String),
    /// A request whose handler may block for minutes, so it runs on the pool.
    Request(PendingRequest),
}

/// Classify one JSON-RPC message (a single line of input): parse it and decide
/// whether it needs a reply, and whether producing that reply needs a tool.
fn classify_line(line: &str) -> Incoming {
    let value: Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(_) => return Incoming::Immediate(error_response(Value::Null, -32700, "parse error")),
    };

    let method = value.get("method").and_then(Value::as_str);
    let id = value.get("id").cloned();
    match (method, id) {
        // A request with an id but no method is malformed (Invalid Request). Echo
        // the client's id so it can correlate the error with its in-flight
        // request — per JSON-RPC the response id is null only when the id cannot
        // be detected, which is not the case here.
        (None, Some(id)) => Incoming::Immediate(error_response(
            id,
            -32600,
            "invalid request: missing method",
        )),
        // No id means a notification: act on it but send no reply. A message with
        // neither method nor id is a malformed notification and likewise gets none
        // (there is no id to correlate a reply against).
        (Some(_), None) | (None, None) => Incoming::Ignored,
        (Some(method), Some(id)) => Incoming::Request(PendingRequest {
            method: method.to_string(),
            params: value.get("params").cloned(),
            id,
        }),
    }
}

/// Handle one JSON-RPC message (a single line of input) for `service`. Returns
/// the JSON response to write back, or `None` for notifications (which carry no
/// id and take no reply).
pub fn dispatch_line(service: &dyn McpService, line: &str) -> Option<String> {
    match classify_line(line) {
        Incoming::Ignored => None,
        Incoming::Immediate(response) => Some(response),
        Incoming::Request(request) => Some(run_request(service, request)),
    }
}

/// Run a classified request through `service`, producing its reply.
fn run_request(service: &dyn McpService, request: PendingRequest) -> String {
    dispatch_request(
        service,
        &request.method,
        request.params.as_ref(),
        request.id,
    )
}

/// Dispatch a request (one that expects a reply) to its handler.
fn dispatch_request(
    service: &dyn McpService,
    method: &str,
    params: Option<&Value>,
    id: Value,
) -> String {
    match method {
        "initialize" => success_response(id, initialize_result(service.server_name())),
        "ping" => success_response(id, json!({})),
        "tools/list" => success_response(id, json!({ "tools": service.tool_schemas() })),
        "tools/call" => dispatch_tool_call(service, params, id),
        other => error_response(id, -32601, &format!("method not found: {other}")),
    }
}

/// Handle `tools/call`: resolve the tool name, run it, and wrap the outcome as
/// MCP tool result content.
fn dispatch_tool_call(service: &dyn McpService, params: Option<&Value>, id: Value) -> String {
    let Some(name) = params.and_then(|p| p.get("name")).and_then(Value::as_str) else {
        return error_response(id, -32602, "invalid params: missing tool name");
    };
    // Per MCP, `arguments` MUST be an object when present. Validate it at the
    // framing layer so a client sending e.g. `"arguments": 42` gets a clear
    // `-32602` rather than a serde type error leaking out of a tool handler. An
    // absent or null `arguments` is treated as the empty object.
    let arguments = match params.and_then(|p| p.get("arguments")) {
        None | Some(Value::Null) => json!({}),
        Some(value @ Value::Object(_)) => value.clone(),
        Some(_) => {
            return error_response(id, -32602, "invalid params: arguments must be an object")
        }
    };

    let panic_arguments = arguments.clone();
    let outcome = match catch_unwind(AssertUnwindSafe(|| service.call_tool(name, arguments))) {
        Ok(outcome) => outcome,
        Err(payload) => {
            let message = panic_payload_message(&*payload);
            log_tool_panic(name, &panic_arguments, &message);
            Err(format!("tool `{name}` panicked: {message}"))
        }
    };
    crate::infrastructure::trace_log::TraceLog::record(
        crate::domain::trace::TraceEvent::now(crate::domain::trace::TraceCategory::Mcp, name)
            .with_detail(if outcome.is_ok() { "ok" } else { "error" }),
    );
    let result = match outcome {
        Ok(text) => json!({ "content": [{ "type": "text", "text": text }], "isError": false }),
        Err(text) => json!({ "content": [{ "type": "text", "text": text }], "isError": true }),
    };
    success_response(id, result)
}

/// Extract a readable message from a panic payload caught while running one MCP
/// tool. Panic payloads are conventionally `&'static str` or `String`; anything
/// else still becomes a stable placeholder so the tool result remains valid JSON
/// and the server keeps serving following requests.
fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

/// Record the full local diagnostic context for a panic caught at one tool's
/// call boundary. The MCP reply stays intentionally short, while the error log
/// gets the tool name, panic payload, request arguments (bounded), and a forced
/// backtrace captured at the catch site so the next crash has enough detail to
/// identify the failing path.
fn log_tool_panic(name: &str, arguments: &Value, message: &str) {
    let backtrace = Backtrace::force_capture();
    crate::infrastructure::error_log::ErrorLog::record(&format!(
        "mcp tool `{name}` panicked: {message}\narguments: {}\nbacktrace:\n{backtrace}",
        arguments_json_for_log(arguments)
    ));
}

fn arguments_json_for_log(arguments: &Value) -> String {
    truncate_for_log(arguments.to_string(), MAX_PANIC_ARGUMENT_CHARS)
}

fn truncate_for_log(text: String, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text;
    }
    let mut truncated = text.chars().take(max_chars).collect::<String>();
    truncated.push_str("…<truncated>");
    truncated
}

/// Wrap `result` as a JSON-RPC success response for `id`.
pub fn success_response(id: Value, result: Value) -> String {
    serde_json::to_string(&json!({ "jsonrpc": "2.0", "id": id, "result": result }))
        .unwrap_or_default()
}

/// Wrap a `code` / `message` pair as a JSON-RPC error response for `id`.
pub fn error_response(id: Value, code: i64, message: &str) -> String {
    serde_json::to_string(
        &json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }),
    )
    .unwrap_or_default()
}

/// The `initialize` result advertising `name` as the server identity.
fn initialize_result(name: &str) -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": name, "version": env!("CARGO_PKG_VERSION") },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Condvar};
    use std::time::{Duration, Instant};

    /// How long a concurrency test waits for replies before failing. Generous:
    /// exceeding it means a request was stalled, not that the machine is slow.
    const REPLY_TIMEOUT: Duration = Duration::from_secs(30);

    /// A minimal service: every tool call echoes its name back, so the loop's
    /// framing can be exercised without any real business logic.
    struct EchoService;

    impl McpService for EchoService {
        fn server_name(&self) -> &str {
            "echo"
        }

        fn tool_schemas(&self) -> Value {
            json!([])
        }

        fn call_tool(&self, name: &str, _arguments: Value) -> Result<String, String> {
            Ok(format!("called {name}"))
        }
    }

    /// A service with one deliberately-panicking tool, used to prove that a bad
    /// tool call is isolated to that one MCP response.
    struct PanickingService;

    impl McpService for PanickingService {
        fn server_name(&self) -> &str {
            "panicky"
        }

        fn tool_schemas(&self) -> Value {
            json!([])
        }

        fn call_tool(&self, name: &str, _arguments: Value) -> Result<String, String> {
            if name == "explode" {
                panic!("boom");
            }
            Ok(format!("called {name}"))
        }
    }

    /// A service whose `slow` tool blocks until the test releases it, so a test
    /// can prove that a later request is answered while an earlier one is still
    /// running. Every other tool name returns immediately.
    struct GatedService {
        /// Signalled once per `slow` call, as soon as that call has begun.
        started: Mutex<Sender<()>>,
        /// Flipped by [`GatedService::release`] to let `slow` calls return.
        open: Mutex<bool>,
        opened: Condvar,
    }

    impl GatedService {
        /// The service plus the stream of "a `slow` call started" signals.
        fn new() -> (Self, Receiver<()>) {
            let (started, starts) = mpsc::channel();
            (
                Self {
                    started: Mutex::new(started),
                    open: Mutex::new(false),
                    opened: Condvar::new(),
                },
                starts,
            )
        }

        /// Let every blocked (and every later) `slow` call return.
        fn release(&self) {
            *self.open.lock().expect("gate lock") = true;
            self.opened.notify_all();
        }
    }

    impl McpService for GatedService {
        fn server_name(&self) -> &str {
            "gated"
        }

        fn tool_schemas(&self) -> Value {
            json!([])
        }

        fn call_tool(&self, name: &str, _arguments: Value) -> Result<String, String> {
            if name != "slow" {
                return Ok(format!("called {name}"));
            }
            self.started
                .lock()
                .expect("start lock")
                .send(())
                .expect("the test watches for starts");
            let mut open = self.open.lock().expect("gate lock");
            while !*open {
                open = self.opened.wait(open).expect("gate wait");
            }
            Ok("called slow".to_string())
        }
    }

    /// A [`Read`] fed one line at a time from a channel, so a test can hold back a
    /// later request until an earlier one is known to be running. EOF arrives only
    /// when the sending half is dropped, which keeps `serve` reading in between.
    struct ChannelReader {
        lines: Receiver<String>,
        pending: Vec<u8>,
    }

    impl Read for ChannelReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.pending.is_empty() {
                let Ok(line) = self.lines.recv() else {
                    return Ok(0);
                };
                self.pending = line.into_bytes();
            }
            let taken = self.pending.len().min(buf.len());
            buf[..taken].copy_from_slice(&self.pending[..taken]);
            self.pending.drain(..taken);
            Ok(taken)
        }
    }

    /// The sending half of a [`ChannelReader`]: feeds request lines to a `serve`
    /// running on another thread, and closes the stream when dropped.
    struct RequestFeed(Sender<String>);

    impl RequestFeed {
        fn new() -> (Self, std::io::BufReader<ChannelReader>) {
            let (sender, lines) = mpsc::channel();
            (
                Self(sender),
                std::io::BufReader::new(ChannelReader {
                    lines,
                    pending: Vec::new(),
                }),
            )
        }

        /// Send a `tools/call` for `tool` carrying JSON-RPC id `id`.
        fn call(&self, id: i64, tool: &str) {
            self.send(&format!(
                r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"{tool}","arguments":{{}}}}}}"#
            ));
        }

        fn send(&self, line: &str) {
            self.0.send(format!("{line}\n")).expect("serve is reading");
        }
    }

    /// Captures replies into a shared buffer so a test can inspect them while
    /// `serve` is still running on another thread.
    #[derive(Clone)]
    struct SharedOutput(Arc<Mutex<Vec<u8>>>);

    impl SharedOutput {
        fn new() -> Self {
            Self(Arc::new(Mutex::new(Vec::new())))
        }

        fn push(&self, bytes: &[u8]) {
            self.0
                .lock()
                .expect("captured output lock")
                .extend_from_slice(bytes);
        }

        /// The replies written so far, each parsed from its own line. A line that
        /// fails to parse means two replies were interleaved.
        fn replies(&self) -> Vec<Value> {
            let bytes = self.0.lock().expect("captured output lock").clone();
            String::from_utf8(bytes)
                .expect("replies are utf-8")
                .lines()
                .map(|line| serde_json::from_str(line).expect("one whole reply per line"))
                .collect()
        }

        /// Wait until a reply carrying `id` has been written, and return it.
        fn wait_for_id(&self, id: i64) -> Value {
            let deadline = Instant::now() + REPLY_TIMEOUT;
            loop {
                let replies = self.replies();
                let found = replies
                    .iter()
                    .find(|reply| reply["id"] == json!(id))
                    .cloned();
                if let Some(reply) = found {
                    return reply;
                }
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for a reply to id {id}, got {replies:?}"
                );
                std::thread::sleep(Duration::from_millis(2));
            }
        }

        /// Wait until at least `count` replies have been written.
        fn wait_for(&self, count: usize) -> Vec<Value> {
            let deadline = Instant::now() + REPLY_TIMEOUT;
            loop {
                let replies = self.replies();
                if replies.len() >= count || Instant::now() >= deadline {
                    assert!(
                        replies.len() >= count,
                        "timed out waiting for {count} replies, got {replies:?}"
                    );
                    return replies;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }

    impl Write for SharedOutput {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.push(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A writer that accepts only a few bytes per call and yields the thread
    /// between them, so a server that did not serialise its writes would visibly
    /// split one reply across lines. Captures into a [`SharedOutput`].
    #[derive(Clone)]
    struct ChunkedOutput(SharedOutput);

    impl Write for ChunkedOutput {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            std::thread::yield_now();
            let chunk = buf.len().min(4);
            self.0.push(&buf[..chunk]);
            Ok(chunk)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A writer whose every write fails, standing in for a client that closed the
    /// other end of the pipe.
    struct FailingOutput;

    impl Write for FailingOutput {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "closed",
            ))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// [`ServeLimits`] with the production line cap and an explicit pool shape.
    fn limits(workers: usize, queue: usize) -> ServeLimits {
        ServeLimits {
            workers,
            queue,
            ..ServeLimits::default()
        }
    }

    /// The ids of `replies`, in the order they were written.
    fn ids(replies: &[Value]) -> Vec<i64> {
        replies
            .iter()
            .map(|reply| reply["id"].as_i64().expect("an integer id"))
            .collect()
    }

    #[test]
    fn serve_replies_to_requests_but_not_to_blank_lines_or_notifications() {
        // A blank line is skipped, and a notification (a message with a method
        // but no id) is acted on without a reply, so only the `ping` request
        // produces a single line of output.
        let input = concat!(
            " \n",
            "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n",
            " \n",
        );
        let mut output = Vec::new();

        serve(&EchoService, input.as_bytes(), &mut output).unwrap();

        let response = String::from_utf8(output).unwrap();
        assert!(response.contains("\"id\":1"));
        assert!(response.contains("\"result\":{}"));
        assert_eq!(response.lines().count(), 1);
    }

    #[test]
    fn serve_advertises_the_service_identity_and_tools() {
        assert!(EchoService.tool_names().is_empty());

        let input = concat!(
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n",
        );
        let mut output = Vec::new();

        serve(&EchoService, input.as_bytes(), &mut output).unwrap();

        let response = String::from_utf8(output).unwrap();
        assert!(response.contains("\"name\":\"echo\""));
        assert!(response.contains("\"tools\":[]"));
    }

    #[test]
    fn missing_method_with_an_id_echoes_that_id_in_the_error() {
        // A request that omits `method` but carries an id is Invalid Request; the
        // error response must echo the id so a strict client can correlate it.
        let response = dispatch_line(&EchoService, r#"{"jsonrpc":"2.0","id":5}"#).expect("a reply");
        let value: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(value["error"]["code"], -32600);
        assert_eq!(value["id"], json!(5));
    }

    #[test]
    fn a_message_with_neither_method_nor_id_gets_no_reply() {
        // No id means nothing to correlate a reply against, so a method-less,
        // id-less message is treated as a malformed notification: no response.
        assert!(dispatch_line(&EchoService, "{}").is_none());
    }

    #[test]
    fn serve_exits_cleanly_on_eof() {
        let mut output = Vec::new();

        let result = serve(&EchoService, "".as_bytes(), &mut output);

        assert!(result.is_ok());
        assert!(output.is_empty());
    }

    #[test]
    fn read_capped_line_reports_eof_lines_and_drains_an_overlong_line() {
        // First line exceeds the 4-byte cap (no newline within 4 bytes); the
        // following short line is then returned intact, proving the over-long
        // line's remainder was drained and the reader resynced on a boundary.
        let mut input = std::io::Cursor::new(b"abcdefgh\nok\n".to_vec());
        let mut raw = Vec::new();
        assert!(matches!(
            read_capped_line(&mut input, &mut raw, 4).unwrap(),
            LineRead::TooLong
        ));
        assert!(matches!(
            read_capped_line(&mut input, &mut raw, 4).unwrap(),
            LineRead::Line
        ));
        assert_eq!(raw, b"ok\n");
        assert!(matches!(
            read_capped_line(&mut input, &mut raw, 4).unwrap(),
            LineRead::Eof
        ));
    }

    #[test]
    fn serve_rejects_an_overlong_line_with_a_parse_error_and_keeps_going() {
        // A line larger than the cap is answered with a parse error rather than
        // buffered without bound, and a valid request after it is still served.
        // The cap (128) comfortably fits the ping below but not the padded line.
        let overlong = format!("{}\n", "x".repeat(200));
        let input = format!("{overlong}{{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"ping\"}}\n");
        let mut output = Vec::new();

        serve_with_limits(
            &EchoService,
            input.as_bytes(),
            &mut output,
            &ServeLimits {
                max_line_bytes: 128,
                ..ServeLimits::default()
            },
        )
        .unwrap();

        let response = String::from_utf8(output).unwrap();
        assert!(response.contains("\"code\":-32700"), "{response}");
        assert!(response.contains("request too large"), "{response}");
        // Exactly one rejection, not one per buffered chunk of the long line.
        assert_eq!(
            response.matches("request too large").count(),
            1,
            "{response}"
        );
        // The ping after the over-long line was still answered.
        assert!(response.contains("\"id\":7"), "{response}");
    }

    #[test]
    fn serve_processes_tool_calls_via_the_service() {
        let request = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"do_thing","arguments":{}}}"#;
        let input = format!("{request}\n");
        let mut output = Vec::new();

        serve(&EchoService, input.as_bytes(), &mut output).unwrap();

        let response = String::from_utf8(output).unwrap();
        assert!(response.contains("called do_thing"));
    }

    #[test]
    fn a_panicking_tool_returns_is_error_and_the_server_keeps_serving() {
        // One tool panic is converted into that call's MCP `isError` result; it
        // must not unwind out of the stdio loop and take every subsequent tool
        // down with it. The next request in the same input stream proves the
        // server stayed alive after the panic.
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"explode","arguments":{}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"after","arguments":{}}}"#,
            "\n",
        );
        let mut output = Vec::new();

        // The service identity/schema accessors are part of the trait surface the
        // stdio loop uses for `initialize` / `tools/list`; touch them so the panic
        // fixture is exercised in full, not only through `call_tool`.
        assert_eq!(PanickingService.server_name(), "panicky");
        assert!(PanickingService
            .tool_schemas()
            .as_array()
            .is_some_and(|tools| tools.is_empty()));

        serve(&PanickingService, input.as_bytes(), &mut output).unwrap();

        let response = String::from_utf8(output).unwrap();
        let replies = response
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(replies.len(), 2);
        // Replies are looked up by id, not by position: requests are dispatched
        // concurrently, so the order they complete in is not the request order.
        let by_id = |wanted: i64| {
            replies
                .iter()
                .find(|reply| reply["id"] == json!(wanted))
                .expect("a reply carrying the requested id")
        };
        let panicked = by_id(1);
        assert_eq!(panicked["result"]["isError"], json!(true));
        assert!(panicked["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("tool `explode` panicked"));
        let after = by_id(2);
        assert_eq!(after["result"]["isError"], json!(false));
        assert_eq!(after["result"]["content"][0]["text"], json!("called after"));
    }

    #[test]
    fn a_panicking_tool_writes_arguments_and_backtrace_to_the_error_log() {
        // Point ErrorLog at a temp home so the panic diagnostic is inspectable
        // without polluting the developer's real `~/.usagi/logs/`.
        let _guard = crate::test_support::process_env_guard();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var(crate::infrastructure::storage::DATA_DIR_ENV, home.path());

        let request = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"explode","arguments":{"flag":true,"prompt":"hello"}}}"#;
        let input = format!("{request}\n");
        let mut output = Vec::new();

        serve(&PanickingService, input.as_bytes(), &mut output).unwrap();

        let log_dir = home.path().join("logs");
        let entry = std::fs::read_dir(&log_dir)
            .expect("logs dir exists")
            .next()
            .expect("a log file was written")
            .expect("readable entry");
        let contents = std::fs::read_to_string(entry.path()).unwrap();
        assert!(contents.contains("mcp tool `explode` panicked: boom"));
        assert!(contents.contains(r#""flag":true"#));
        assert!(contents.contains(r#""prompt":"hello""#));
        assert!(contents.contains("backtrace:"));
        assert!(contents.contains("log_tool_panic"));

        std::env::remove_var(crate::infrastructure::storage::DATA_DIR_ENV);
    }

    #[test]
    fn serve_answers_a_non_utf8_line_with_a_parse_error_and_keeps_going() {
        // A line of invalid UTF-8 must not terminate the server: it becomes a
        // parse error, and a following valid request is still answered.
        let mut input: Vec<u8> = Vec::new();
        input.extend_from_slice(&[0xff, 0xfe, 0x00, b'\n']); // not valid UTF-8
        input.extend_from_slice(b"{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"ping\"}\n");
        let mut output = Vec::new();

        serve(&EchoService, input.as_slice(), &mut output).unwrap();

        let response = String::from_utf8(output).unwrap();
        // Two replies: the parse error for the bad line, then the ping result.
        assert!(response.contains("-32700"));
        assert!(response.contains("\"id\":7"));
        assert_eq!(response.lines().count(), 2);
    }

    #[test]
    fn tool_call_with_non_object_arguments_is_an_invalid_params_error() {
        // `arguments` present but not an object is rejected at the framing layer.
        let request = r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"do_thing","arguments":42}}"#;
        let input = format!("{request}\n");
        let mut output = Vec::new();

        serve(&EchoService, input.as_bytes(), &mut output).unwrap();

        let response = String::from_utf8(output).unwrap();
        assert!(response.contains("-32602"));
        assert!(response.contains("arguments must be an object"));
        // The tool was never reached.
        assert!(!response.contains("called do_thing"));
    }

    #[test]
    fn into_schema_array_takes_arrays_and_degrades_other_shapes_to_empty() {
        // An array is unwrapped to its entries…
        assert_eq!(
            into_schema_array(json!([{"name": "a"}, {"name": "b"}])).len(),
            2
        );
        // …and any non-array (a construction bug) degrades to no entries rather
        // than panicking the `tools/list` path.
        assert!(into_schema_array(json!({"not": "an array"})).is_empty());
        assert!(into_schema_array(json!(null)).is_empty());
    }

    #[test]
    fn tool_call_with_null_arguments_is_treated_as_empty() {
        // An explicit null `arguments` is lenient — the same as omitting it.
        let request = r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"do_thing","arguments":null}}"#;
        let input = format!("{request}\n");
        let mut output = Vec::new();

        serve(&EchoService, input.as_bytes(), &mut output).unwrap();

        let response = String::from_utf8(output).unwrap();
        assert!(response.contains("called do_thing"));
    }

    #[test]
    fn panic_payload_message_covers_common_and_opaque_payloads() {
        let borrowed: Box<dyn std::any::Any + Send> = Box::new("borrowed");
        assert_eq!(panic_payload_message(&*borrowed), "borrowed");

        let owned: Box<dyn std::any::Any + Send> = Box::new(String::from("owned"));
        assert_eq!(panic_payload_message(&*owned), "owned");

        let opaque: Box<dyn std::any::Any + Send> = Box::new(123_u32);
        assert_eq!(panic_payload_message(&*opaque), "non-string panic payload");
    }

    #[test]
    fn a_slow_tool_call_does_not_stall_a_later_request_on_the_same_stream() {
        // The regression this whole pool exists for: `session_remove` took minutes
        // and every following request on the same stdio connection — including
        // read-only ones — waited for it. The slow call stays blocked while a later
        // request is answered.
        let (service, starts) = GatedService::new();
        let (feed, input) = RequestFeed::new();
        let output = SharedOutput::new();
        let pool = limits(2, 8);

        std::thread::scope(|scope| {
            let server = scope.spawn(|| serve_with_limits(&service, input, output.clone(), &pool));

            feed.call(1, "slow");
            starts.recv().expect("the slow call started");
            feed.call(2, "fast");

            // Answered while id 1 is still inside the gate, so replies come out in
            // completion order rather than request order.
            let replies = output.wait_for(1);
            assert_eq!(ids(&replies), vec![2]);

            service.release();
            drop(feed);
            server
                .join()
                .expect("the serve thread finished")
                .expect("serve succeeded");
        });

        assert_eq!(ids(&output.replies()), vec![2, 1]);
    }

    #[test]
    fn out_of_order_replies_keep_their_own_request_ids() {
        // Every request calls a differently-named tool, so each reply proves it
        // carries the id of the request that produced *it* — not merely that some
        // reply arrived for every id.
        let (service, _starts) = GatedService::new();
        let (feed, input) = RequestFeed::new();
        let output = SharedOutput::new();
        let pool = limits(4, 16);

        std::thread::scope(|scope| {
            let server = scope.spawn(|| serve_with_limits(&service, input, output.clone(), &pool));
            for id in 1..=12 {
                feed.call(id, &format!("tool{id}"));
            }
            drop(feed);
            server
                .join()
                .expect("the serve thread finished")
                .expect("serve succeeded");
        });

        // `serve` returned, so every in-flight request has been answered.
        let replies = output.replies();
        assert_eq!(replies.len(), 12);
        for reply in &replies {
            let id = reply["id"].as_i64().expect("an integer id");
            assert_eq!(
                reply["result"]["content"][0]["text"],
                json!(format!("called tool{id}"))
            );
        }
        let mut seen = ids(&replies);
        seen.sort_unstable();
        assert_eq!(seen, (1..=12).collect::<Vec<_>>());
    }

    #[test]
    fn requests_beyond_the_bounded_pool_are_refused_and_the_server_keeps_serving() {
        // One worker plus one queue slot admits two requests; a third is refused
        // explicitly instead of being buffered without bound. The bound counts
        // admitted-but-unfinished requests, so the refusal does not depend on
        // whether a worker has picked the queued request up yet.
        let (service, starts) = GatedService::new();
        let (feed, input) = RequestFeed::new();
        let output = SharedOutput::new();
        let pool = limits(1, 1);

        std::thread::scope(|scope| {
            let server = scope.spawn(|| serve_with_limits(&service, input, output.clone(), &pool));

            feed.call(1, "slow");
            starts.recv().expect("the first slow call started");
            feed.call(2, "slow");
            feed.call(3, "slow");

            // The refusal is written by the reading thread, so it lands while both
            // admitted requests are still in flight.
            let replies = output.wait_for(1);
            assert_eq!(ids(&replies), vec![3]);
            assert_eq!(replies[0]["error"]["code"], json!(SERVER_BUSY_CODE));
            assert!(
                replies[0]["error"]["message"]
                    .as_str()
                    .expect("an error message")
                    .contains("server busy: 2 requests already in flight"),
                "{replies:?}"
            );

            // The server survived the refusal: with the gate open the admitted
            // requests drain and a later request is served normally. `-32000` asks
            // the client to resend, and a resend can itself race the release of the
            // slot it needs, so retry the way a real client would.
            service.release();
            let mut id = 4;
            let served = loop {
                feed.call(id, "fast");
                let reply = output.wait_for_id(id);
                if reply["error"]["code"] != json!(SERVER_BUSY_CODE) {
                    break reply;
                }
                id += 1;
            };
            assert_eq!(served["result"]["content"][0]["text"], json!("called fast"));

            drop(feed);
            server
                .join()
                .expect("the serve thread finished")
                .expect("serve succeeded");
        });

        // Both admitted requests ran to completion despite the refusal beside them.
        for admitted in [1, 2] {
            assert_eq!(
                output.wait_for_id(admitted)["result"]["content"][0]["text"],
                json!("called slow")
            );
        }
    }

    #[test]
    fn concurrent_replies_are_written_as_whole_lines() {
        // The output takes only 4 bytes per call and yields the thread between
        // them, so a server that did not serialise its writes would split replies
        // across lines. `replies` parses every line, so any interleaving fails here.
        let (service, _starts) = GatedService::new();
        let (feed, input) = RequestFeed::new();
        let captured = SharedOutput::new();
        let output = ChunkedOutput(captured.clone());
        let pool = limits(8, 16);

        std::thread::scope(|scope| {
            let server = scope.spawn(|| serve_with_limits(&service, input, output.clone(), &pool));
            for id in 1..=16 {
                feed.call(id, &format!("tool{id}"));
            }
            drop(feed);
            server
                .join()
                .expect("the serve thread finished")
                .expect("serve succeeded");
        });

        assert_eq!(captured.replies().len(), 16);
    }

    #[test]
    fn serve_stops_reading_once_a_reply_cannot_be_written() {
        // The parse error for the first line is written by the reading thread
        // itself, so the failure is recorded before the next line is read. The loop
        // then stops even though the input stream is still open — no further tools
        // run for a client that is gone — and `serve` reports the write error.
        let (feed, input) = RequestFeed::new();

        let error = std::thread::scope(|scope| {
            let server = scope.spawn(|| serve(&EchoService, input, FailingOutput));
            feed.send("not json");
            server.join().expect("the serve thread finished")
        })
        .expect_err("the write error is reported");

        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn response_writer_keeps_the_first_write_error_and_drops_later_replies() {
        let writer = ResponseWriter::new(FailingOutput);

        writer.write_line("{}");
        assert!(writer.failed());
        // A reply after the failure is dropped rather than retried: the stream is
        // gone, and the first error is the one `serve` reports.
        writer.write_line("{}");

        let error = writer.into_error().expect("the write error was kept");
        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn admission_bounds_requests_in_flight_and_reuses_released_slots() {
        let admission = Admission::new(2);

        assert!(admission.try_admit());
        assert!(admission.try_admit());
        // Saturated: the next request is refused rather than queued.
        assert!(!admission.try_admit());

        admission.release();
        assert!(admission.try_admit());
    }

    #[test]
    fn panic_argument_dump_is_bounded_but_keeps_short_arguments_intact() {
        assert_eq!(truncate_for_log("short".to_string(), 10), "short");
        assert_eq!(
            truncate_for_log("あいうえお".to_string(), 3),
            "あいう…<truncated>"
        );
        let arguments = json!({ "prompt": "hello" });
        assert_eq!(arguments_json_for_log(&arguments), r#"{"prompt":"hello"}"#);
    }
}
