//! Bounded, document-only protocol for Windows launch forwarding. Kept transport-independent
//! so malformed requests, partial I/O and timeouts can be regression-tested on every platform.

use std::io::{self, Read, Write};
use std::path::Path;
use std::time::{Duration, Instant};

pub const MAX_MESSAGE: usize = 128 * 1024;
const MAX_FILES: usize = 256;
pub const IO_TIMEOUT: Duration = Duration::from_secs(2);
pub const POLL_INTERVAL: Duration = Duration::from_millis(10);

pub fn encode(paths: &[String]) -> io::Result<Vec<u8>> {
    validate(paths)?;
    let bytes = serde_json::to_vec(&serde_json::json!({"version": 1, "paths": paths}))?;
    if bytes.len() > MAX_MESSAGE {
        return Err(invalid("too many document path bytes (maximum 128 KiB)"));
    }
    Ok(bytes)
}

pub fn decode(bytes: &[u8]) -> io::Result<Vec<String>> {
    if bytes.len() > MAX_MESSAGE {
        return Err(invalid("document request exceeds 128 KiB"));
    }
    let value: serde_json::Value = serde_json::from_slice(bytes)?;
    let object = value.as_object().ok_or_else(|| invalid("expected a document request"))?;
    if object.len() != 2 || object.get("version").and_then(|v| v.as_u64()) != Some(1) {
        return Err(invalid("unsupported document request (only version and paths are allowed)"));
    }
    let paths = object.get("paths").and_then(|v| v.as_array()).ok_or_else(|| invalid("expected document paths"))?;
    if paths.len() > MAX_FILES {
        return Err(invalid("at most 256 documents can be opened in one launch"));
    }
    let paths =
        paths.iter().map(|v| v.as_str().map(str::to_owned).ok_or_else(|| invalid("expected a path string"))).collect::<io::Result<Vec<_>>>()?;
    validate(&paths)?;
    Ok(paths)
}

fn validate(paths: &[String]) -> io::Result<()> {
    if paths.len() > MAX_FILES {
        return Err(invalid("at most 256 documents can be opened in one launch"));
    }
    let size = paths.iter().try_fold(0usize, |size, p| size.checked_add(p.len()));
    if size.is_none_or(|size| size > MAX_MESSAGE) {
        return Err(invalid("document request exceeds 128 KiB"));
    }
    if paths.iter().any(|p| p.contains('\0') || !Path::new(p).is_absolute()) {
        return Err(invalid("document paths must be absolute and contain no NUL characters"));
    }
    Ok(())
}

/// Resolve paths in the launching process, not in the existing window's working directory.
pub fn absolute_paths(paths: &[String]) -> io::Result<Vec<String>> {
    paths
        .iter()
        .map(|p| {
            let path = std::path::absolute(p)?;
            path.into_os_string().into_string().map_err(|_| invalid("document path is not valid Unicode"))
        })
        .collect()
}

pub fn write_frame(stream: &mut impl Write, bytes: &[u8], deadline: Instant) -> io::Result<()> {
    if bytes.len() > MAX_MESSAGE {
        return Err(invalid("document request exceeds 128 KiB"));
    }
    let len = u32::try_from(bytes.len()).map_err(|_| invalid("document request too large"))?;
    write_all(stream, &len.to_le_bytes(), deadline)?;
    write_all(stream, bytes, deadline)
}

pub fn read_frame(stream: &mut impl Read, deadline: Instant) -> io::Result<Vec<u8>> {
    let mut length = [0; 4];
    read_exact(stream, &mut length, deadline)?;
    let length = u32::from_le_bytes(length) as usize;
    if length > MAX_MESSAGE {
        return Err(invalid("document request exceeds 128 KiB"));
    }
    let mut bytes = vec![0; length];
    read_exact(stream, &mut bytes, deadline)?;
    Ok(bytes)
}

pub fn read_exact(stream: &mut impl Read, mut bytes: &mut [u8], deadline: Instant) -> io::Result<()> {
    while !bytes.is_empty() {
        check_deadline(deadline)?;
        match stream.read(bytes) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => bytes = bytes.get_mut(n..).ok_or_else(|| invalid("invalid read length"))?,
            Err(e) if retry_io(&e) => std::thread::sleep(POLL_INTERVAL),
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

pub fn write_all(stream: &mut impl Write, mut bytes: &[u8], deadline: Instant) -> io::Result<()> {
    while !bytes.is_empty() {
        check_deadline(deadline)?;
        match stream.write(bytes) {
            // A nonblocking transport can make no progress while its send buffer is full.
            Ok(0) => std::thread::sleep(POLL_INTERVAL),
            Ok(n) => bytes = bytes.get(n..).ok_or_else(|| invalid("invalid write length"))?,
            Err(e) if retry_io(&e) => std::thread::sleep(POLL_INTERVAL),
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn retry_io(error: &io::Error) -> bool {
    matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted)
}

fn check_deadline(deadline: Instant) -> io::Result<()> {
    if Instant::now() >= deadline {
        Err(io::Error::new(io::ErrorKind::TimedOut, "PdfCraft document forwarding timed out; retry after the existing window responds"))
    } else {
        Ok(())
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn document_batches_round_trip_without_interpreting_options() {
        let paths = absolute_paths(&["one with spaces.pdf".into(), "日本語.pdf".into(), "--mode.pdf".into()]).unwrap();
        assert_eq!(decode(&encode(&paths).unwrap()).unwrap(), paths);
        assert!(decode(br#"{"version":1,"paths":[],"command":"quit"}"#).is_err());
        assert!(decode(br#"{"version":2,"paths":[]}"#).is_err());
        assert!(decode(br#"{"version":1,"paths":["relative.pdf"]}"#).is_err());
        assert!(decode(br#"{"version":1,"paths":[42]}"#).is_err());
        assert!(decode(b"not json").is_err());
        assert!(decode(&encode(&[]).unwrap()).unwrap().is_empty());
    }

    #[test]
    fn size_and_count_limits_are_checked_before_receiving_payloads() {
        let path = absolute_paths(&["a.pdf".into()]).unwrap().remove(0);
        assert!(encode(&vec![path.clone(); MAX_FILES + 1]).is_err());
        assert!(encode(&[format!("{path}{}", "x".repeat(MAX_MESSAGE))]).is_err());
        assert!(encode(&[format!("{path}\0")]).is_err());
        let mut oversized = Cursor::new(u32::MAX.to_le_bytes());
        assert_eq!(read_frame(&mut oversized, Instant::now() + IO_TIMEOUT).unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert!(read_frame(&mut Cursor::new([8, 0, 0, 0, 1]), Instant::now() + IO_TIMEOUT).is_err());
    }

    #[test]
    fn framed_batches_and_expired_deadlines() {
        let bytes = encode(&absolute_paths(&["a.pdf".into(), "b.pdf".into()]).unwrap()).unwrap();
        let mut wire = Vec::new();
        write_frame(&mut wire, &bytes, Instant::now() + IO_TIMEOUT).unwrap();
        assert_eq!(read_frame(&mut Cursor::new(&wire), Instant::now() + IO_TIMEOUT).unwrap(), bytes);
        assert_eq!(read_frame(&mut Cursor::new(&wire), Instant::now()).unwrap_err().kind(), io::ErrorKind::TimedOut);
        assert_eq!(write_frame(&mut Vec::new(), &bytes, Instant::now()).unwrap_err().kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn partial_io_and_backpressure_do_not_truncate_batches_or_wait_forever() {
        struct Short<T>(T);
        impl<T: Read> Read for Short<T> {
            fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
                let len = bytes.len().min(2);
                self.0.read(&mut bytes[..len])
            }
        }
        impl<T: Write> Write for Short<T> {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.0.write(&bytes[..bytes.len().min(3)])
            }
            fn flush(&mut self) -> io::Result<()> {
                self.0.flush()
            }
        }
        struct Blocked;
        impl Read for Blocked {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::ErrorKind::WouldBlock.into())
            }
        }
        impl Write for Blocked {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Ok(0)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let bytes = encode(&absolute_paths(&["a.pdf".into()]).unwrap()).unwrap();
        let mut writer = Short(Vec::new());
        write_frame(&mut writer, &bytes, Instant::now() + IO_TIMEOUT).unwrap();
        assert_eq!(read_frame(&mut Short(Cursor::new(writer.0)), Instant::now() + IO_TIMEOUT).unwrap(), bytes);
        assert_eq!(read_frame(&mut Blocked, Instant::now() + POLL_INTERVAL).unwrap_err().kind(), io::ErrorKind::TimedOut);
        assert_eq!(write_frame(&mut Blocked, &bytes, Instant::now() + POLL_INTERVAL).unwrap_err().kind(), io::ErrorKind::TimedOut);
    }
}
