//! The daemon's IPC request handling: the server side of the
//! [`usagi_core::infrastructure::ipc`] protocol.
//!
//! [`dispatch`] maps one [`Request`] to its [`Response`] — pure application
//! logic. [`handle_connection`] drives a single client connection: it reads
//! framed requests until the client closes, decoding each request, dispatching
//! it, and writing the encoded reply. The reader and writer are injected, so
//! this is fully testable; the synthesis root accepts the real socket and hands
//! its halves here.

use std::io::{self, Read, Write};

use usagi_core::domain::AppInfo;
use usagi_core::infrastructure::ipc::{Request, Response, read_frame, write_frame};

/// Produce the daemon's reply to a single request.
#[must_use]
pub fn dispatch(request: &Request, info: &AppInfo) -> Response {
    match request {
        Request::Ping => Response::Pong {
            version: info.version.to_string(),
        },
    }
}

/// Serve one client connection: decode each framed request, dispatch it, and
/// write the framed reply, until the client closes the connection.
///
/// # Errors
///
/// Returns the transport read/write error, or [`io::ErrorKind::InvalidData`]
/// when a request frame is not a valid [`Request`].
///
/// # Panics
///
/// Never in practice: serializing a [`Response`] cannot fail for its fields.
pub fn handle_connection<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    info: &AppInfo,
) -> io::Result<()> {
    while let Some(payload) = read_frame(reader)? {
        let request: Request = serde_json::from_slice(&payload)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let reply =
            serde_json::to_vec(&dispatch(&request, info)).expect("Response serializes to JSON");
        write_frame(writer, &reply)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{dispatch, handle_connection};
    use std::io::Cursor;
    use usagi_core::domain::AppInfo;
    use usagi_core::infrastructure::ipc::{Request, Response, read_frame, write_frame};

    fn info() -> AppInfo {
        AppInfo {
            name: "usagi",
            version: "0.1.0",
        }
    }

    /// Frame a request as the wire would carry it.
    fn framed(request: &Request) -> Vec<u8> {
        let mut buf = Vec::new();
        write_frame(&mut buf, &serde_json::to_vec(request).unwrap()).unwrap();
        buf
    }

    #[test]
    fn dispatch_answers_ping_with_the_version() {
        assert_eq!(
            dispatch(&Request::Ping, &info()),
            Response::Pong {
                version: "0.1.0".to_string()
            }
        );
    }

    #[test]
    fn handle_connection_replies_to_each_request_until_close() {
        let mut requests = framed(&Request::Ping);
        requests.extend(framed(&Request::Ping));

        let mut reader = Cursor::new(requests);
        let mut writer = Vec::new();
        handle_connection(&mut reader, &mut writer, &info()).unwrap();

        // Two framed pongs come back, then a clean close.
        let mut replies = Cursor::new(writer);
        for _ in 0..2 {
            let payload = read_frame(&mut replies).unwrap().unwrap();
            let response: Response = serde_json::from_slice(&payload).unwrap();
            assert_eq!(
                response,
                Response::Pong {
                    version: "0.1.0".to_string()
                }
            );
        }
        assert_eq!(read_frame(&mut replies).unwrap(), None);
    }

    #[test]
    fn handle_connection_returns_ok_on_an_immediately_closed_connection() {
        let mut reader = Cursor::new(Vec::new());
        let mut writer = Vec::new();
        handle_connection(&mut reader, &mut writer, &info()).unwrap();
        assert!(writer.is_empty());
    }

    #[test]
    fn handle_connection_rejects_a_malformed_request_frame() {
        // A well-framed payload that is not a valid Request.
        let mut reader = Cursor::new({
            let mut buf = Vec::new();
            write_frame(&mut buf, b"not json").unwrap();
            buf
        });
        let mut writer = Vec::new();
        assert!(handle_connection(&mut reader, &mut writer, &info()).is_err());
    }
}
