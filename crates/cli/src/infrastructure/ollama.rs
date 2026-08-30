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

pub struct OllamaClient;

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
        ask_at(OLLAMA_ADDR, model, prompt)
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
    .map_err(|error| error.to_string())?;
    let mut stream = TcpStream::connect_timeout(&address, CONNECT_TIMEOUT).map_err(|error| {
        format!("cannot connect to Ollama at 127.0.0.1:11434; install/start Ollama and pull the requested model: {error}")
    })?;
    stream
        .set_read_timeout(Some(IO_TIMEOUT))
        .and_then(|()| stream.set_write_timeout(Some(IO_TIMEOUT)))
        .map_err(|error| format!("cannot configure Ollama socket: {error}"))?;
    write!(
        stream,
        "POST /api/chat HTTP/1.0\r\nHost: 127.0.0.1:11434\r\nContent-Type: application/json\r\nAccept: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .and_then(|()| stream.write_all(&body))
    .map_err(|error| format!("failed to send request to Ollama: {error}"))?;

    let raw = read_bounded(&mut stream)?;
    parse_http_response(&raw)
}

fn read_bounded(stream: &mut TcpStream) -> Result<Vec<u8>, String> {
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
            let mut buffer = [0_u8; 1024];
            let count = socket.read(&mut buffer).unwrap();
            if count == 0 {
                break;
            }
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
        let answer = ask_at(address, "gemma3:4b", "review this design").unwrap();
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
        assert_port(&OllamaClient);
    }
}
