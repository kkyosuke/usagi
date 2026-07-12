use std::io::{self, Cursor, Read};

use super::{Request, Response, read_frame, write_frame};

#[test]
fn frames_round_trip_back_to_back() {
    let mut buf = Vec::new();
    write_frame(&mut buf, b"first").unwrap();
    write_frame(&mut buf, b"second").unwrap();

    let mut cursor = Cursor::new(buf);
    assert_eq!(read_frame(&mut cursor).unwrap(), Some(b"first".to_vec()));
    assert_eq!(read_frame(&mut cursor).unwrap(), Some(b"second".to_vec()));
    // A clean close at the frame boundary yields None rather than an error.
    assert_eq!(read_frame(&mut cursor).unwrap(), None);
}

#[test]
fn messages_serialize_through_a_frame() {
    // The protocol enums travel as JSON payloads inside frames.
    let mut buf = Vec::new();
    write_frame(&mut buf, &serde_json::to_vec(&Request::Ping).unwrap()).unwrap();
    let reply = Response::Pong {
        version: "0.1.0".to_string(),
    };
    write_frame(&mut buf, &serde_json::to_vec(&reply).unwrap()).unwrap();

    let mut cursor = Cursor::new(buf);
    let request: Request =
        serde_json::from_slice(&read_frame(&mut cursor).unwrap().unwrap()).unwrap();
    let response: Response =
        serde_json::from_slice(&read_frame(&mut cursor).unwrap().unwrap()).unwrap();
    assert_eq!(request, Request::Ping);
    assert_eq!(response, reply);
}

#[test]
fn exercises_message_derives() {
    let request = Request::Ping;
    assert_eq!(request.clone(), request);
    assert!(format!("{request:?}").contains("Ping"));
    let reply = Response::Pong {
        version: "1".to_string(),
    };
    assert_eq!(reply.clone(), reply);
    assert!(format!("{reply:?}").contains("Pong"));
}

#[test]
fn read_propagates_a_truncated_frame_error() {
    // Length prefix claims 10 bytes but only 3 follow: a truncated frame is an
    // error, distinct from a clean close.
    let mut buf = 10u32.to_be_bytes().to_vec();
    buf.extend_from_slice(b"abc");
    let mut cursor = Cursor::new(buf);
    assert!(read_frame(&mut cursor).is_err());
}

#[test]
fn read_propagates_a_reader_error() {
    struct FailingReader;
    impl Read for FailingReader {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("read failed"))
        }
    }
    assert!(read_frame(&mut FailingReader).is_err());
}

#[test]
fn write_propagates_a_writer_error() {
    struct FailingWriter;
    impl io::Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("write failed"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    assert!(write_frame(&mut FailingWriter, b"payload").is_err());
}
