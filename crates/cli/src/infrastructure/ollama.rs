//! Bounded localhost HTTP adapter for Ollama's chat API, owned by the CLI face.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::usecase::local_opinion::{LocalOpinion, LocalOpinionPort};

const OLLAMA_ADDR: SocketAddr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 11_434));
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const IO_TIMEOUT: Duration = Duration::from_secs(120);
const REVIEWER_SYSTEM_PROMPT: &str = "You are an independent third-opinion reviewer. Analyze the question critically, state assumptions, identify risks or counterarguments, and give a concise recommendation. Do not claim access to tools or context that was not provided.";

pub struct OllamaClient {
    address: SocketAddr,
}

impl Default for OllamaClient {
    fn default() -> Self {
        Self {
            address: OLLAMA_ADDR,
        }
    }
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: [ChatMessage<'a>; 2],
    stream: bool,
}

#[derive(Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct ChatResponse {
    model: String,
    message: ResponseMessage,
}

#[derive(Deserialize)]
struct ResponseMessage {
    content: String,
}

#[derive(Deserialize)]
struct ErrorResponse {
    error: Option<String>,
}

impl LocalOpinionPort for OllamaClient {
    fn ask(&self, model: &str, prompt: &str) -> Result<LocalOpinion, String> {
        ask_at(self.address, model, prompt)
    }
}

fn ask_at(address: SocketAddr, model: &str, prompt: &str) -> Result<LocalOpinion, String> {
    if !address.ip().is_loopback() {
        return Err("Ollama endpoint must be a loopback address".into());
    }
    let body = serde_json::to_vec(&ChatRequest {
        model,
        messages: [
            ChatMessage {
                role: "system",
                content: REVIEWER_SYSTEM_PROMPT,
            },
            ChatMessage {
                role: "user",
                content: prompt,
            },
        ],
        stream: false,
    })
    .expect("a chat request containing only strings must serialize");
    let mut stream = connect(address, CONNECT_TIMEOUT)?;
    configure_timeouts(&stream, IO_TIMEOUT, IO_TIMEOUT)?;
    write_request(&mut stream, &body)?;

    let raw = read_bounded(&mut stream)?;
    parse_http_response(&raw)
}

fn connect(address: SocketAddr, timeout: Duration) -> Result<TcpStream, String> {
    match TcpStream::connect_timeout(&address, timeout) {
        Ok(stream) => Ok(stream),
        Err(error) => Err(format!(
            "cannot connect to Ollama at {address}; install/start Ollama and pull the requested model: {error}"
        )),
    }
}

fn configure_timeouts(
    stream: &TcpStream,
    read_timeout: Duration,
    write_timeout: Duration,
) -> Result<(), String> {
    if let Err(error) = stream.set_read_timeout(Some(read_timeout)) {
        return Err(format!("cannot configure Ollama socket: {error}"));
    }
    if let Err(error) = stream.set_write_timeout(Some(write_timeout)) {
        return Err(format!("cannot configure Ollama socket: {error}"));
    }
    Ok(())
}

fn write_request(writer: &mut impl Write, body: &[u8]) -> Result<(), String> {
    let headers = format!(
        "POST /api/chat HTTP/1.0\r\nHost: 127.0.0.1:11434\r\nContent-Type: application/json\r\nAccept: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    if let Err(error) = writer.write_all(headers.as_bytes()) {
        return Err(format!("failed to send request to Ollama: {error}"));
    }
    if let Err(error) = writer.write_all(body) {
        return Err(format!("failed to send request to Ollama: {error}"));
    }
    Ok(())
}

fn read_bounded(stream: &mut impl Read) -> Result<Vec<u8>, String> {
    let mut raw = Vec::new();
    let mut limited = stream.take((MAX_RESPONSE_BYTES + 1) as u64);
    loop {
        let mut buffer = [0_u8; 8192];
        match limited.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => raw.extend_from_slice(&buffer[..count]),
            Err(error)
                if error.kind() == std::io::ErrorKind::ConnectionReset && !raw.is_empty() =>
            {
                break;
            }
            Err(error) => return Err(format!("failed to read Ollama response: {error}")),
        }
    }
    if raw.len() > MAX_RESPONSE_BYTES {
        return Err(format!(
            "Ollama response exceeds {MAX_RESPONSE_BYTES} bytes"
        ));
    }
    Ok(raw)
}

fn parse_http_response(raw: &[u8]) -> Result<LocalOpinion, String> {
    let Some(header_end) = raw.windows(4).position(|window| window == b"\r\n\r\n") else {
        return Err("Ollama returned an invalid HTTP response".into());
    };
    let headers = std::str::from_utf8(&raw[..header_end])
        .map_err(|_| "Ollama returned non-UTF-8 HTTP headers")?;
    let status = headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or("Ollama returned an invalid HTTP status")?;
    let body = &raw[header_end + 4..];
    if status != 200 {
        let detail = serde_json::from_slice::<ErrorResponse>(body)
            .ok()
            .and_then(|response| response.error)
            .unwrap_or_else(|| String::from_utf8_lossy(body).trim().to_owned());
        return Err(if detail.is_empty() {
            format!("Ollama returned HTTP {status}")
        } else {
            format!("Ollama returned HTTP {status}: {detail}")
        });
    }
    let response: ChatResponse = serde_json::from_slice(body)
        .map_err(|error| format!("Ollama returned invalid JSON: {error}"))?;
    if response.message.content.is_empty() {
        return Err("Ollama returned an empty opinion".into());
    }
    Ok(LocalOpinion {
        model: response.model,
        content: response.message.content,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    struct FailingWriter {
        writes: usize,
        fail_on: usize,
    }

    impl Write for FailingWriter {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.writes += 1;
            if self.writes == self.fail_on {
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "closed",
                ))
            } else {
                Ok(buffer.len())
            }
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct ErrorReader;

    impl Read for ErrorReader {
        fn read(&mut self, _buffer: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("failed"))
        }
    }

    struct ResetReader(bool);

    impl Read for ResetReader {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            if self.0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "reset",
                ));
            }
            self.0 = true;
            buffer[..7].copy_from_slice(b"partial");
            Ok(7)
        }
    }

    fn server(response: Vec<u8>) -> (SocketAddr, thread::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let worker = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let request = read_request(&mut socket);
            socket.write_all(&response).unwrap();
            request
        });
        (address, worker)
    }

    fn read_request(socket: &mut TcpStream) -> Vec<u8> {
        let mut request = Vec::new();
        loop {
            // Read one byte at a time so the fixture deterministically exercises the
            // incomplete-header path on every platform.
            let mut buffer = [0_u8; 1];
            let count = socket.read(&mut buffer).unwrap();
            assert_ne!(count, 0, "client closed before sending a complete request");
            request.extend_from_slice(&buffer[..count]);
            let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n")
            else {
                continue;
            };
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            if request.len() >= header_end + 4 + content_length {
                break;
            }
        }
        request
    }

    fn http(status: &str, body: &str) -> Vec<u8> {
        format!("HTTP/1.0 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len()).into_bytes()
    }

    #[test]
    fn asks_loopback_ollama_without_streaming() {
        let (address, worker) = server(http(
            "200 OK",
            r#"{"model":"gemma3:4b","message":{"content":"check the rollback path"}}"#,
        ));
        let answer = OllamaClient { address }
            .ask("gemma3:4b", "review this design")
            .unwrap();
        assert_eq!(answer.model, "gemma3:4b");
        assert_eq!(answer.content, "check the rollback path");
        let request = String::from_utf8(worker.join().unwrap()).unwrap();
        assert!(request.starts_with("POST /api/chat HTTP/1.0"));
        assert!(request.contains(r#""stream":false"#));
        assert!(request.contains("independent third-opinion reviewer"));
    }

    #[test]
    fn rejects_non_loopback_and_unavailable_endpoints() {
        assert!(ask_at("192.0.2.1:11434".parse().unwrap(), "m", "p").is_err());
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        assert!(
            ask_at(address, "m", "p")
                .unwrap_err()
                .contains("cannot connect")
        );
    }

    #[test]
    fn timeout_configuration_and_write_failures_are_mapped() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let worker = thread::spawn(move || listener.accept().unwrap().0);
        let stream = connect(address, CONNECT_TIMEOUT).unwrap();
        let peer = worker.join().unwrap();
        assert!(configure_timeouts(&stream, Duration::ZERO, IO_TIMEOUT).is_err());
        assert!(configure_timeouts(&stream, IO_TIMEOUT, Duration::ZERO).is_err());
        drop(peer);

        for fail_on in [1, 2] {
            assert!(write_request(&mut FailingWriter { writes: 0, fail_on }, b"body").is_err());
        }
        FailingWriter {
            writes: 0,
            fail_on: usize::MAX,
        }
        .flush()
        .unwrap();
    }

    #[test]
    fn read_failures_and_resets_are_mapped() {
        assert!(
            read_bounded(&mut ErrorReader)
                .unwrap_err()
                .contains("failed to read")
        );

        assert_eq!(read_bounded(&mut ResetReader(false)).unwrap(), b"partial");
    }

    #[test]
    fn maps_http_and_wire_failures() {
        for response in [
            http("404 Not Found", r#"{"error":"model not found"}"#),
            http("500 Error", ""),
            http("200 OK", "not json"),
            http("200 OK", r#"{"model":"m","message":{"content":""}}"#),
            b"HTTP/1.0 nope\r\n\r\n".to_vec(),
            [b"HTTP/1.0 200 OK\r\nX: ".as_slice(), &[0xff], b"\r\n\r\n"].concat(),
            b"not http".to_vec(),
        ] {
            let (address, worker) = server(response);
            assert!(ask_at(address, "m", "p").is_err());
            worker.join().unwrap();
        }
    }

    #[test]
    fn rejects_a_response_over_the_hard_limit() {
        let (address, worker) = server(vec![b'x'; MAX_RESPONSE_BYTES + 1]);
        assert!(ask_at(address, "m", "p").unwrap_err().contains("exceeds"));
        worker.join().unwrap();
    }

    #[test]
    fn default_client_implements_the_port() {
        fn assert_port(_: &dyn LocalOpinionPort) {}
        assert_port(&OllamaClient::default());
    }
}
