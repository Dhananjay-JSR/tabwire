//! A minimal WebSocket client for the browser's DevTools socket.
//!
//! The peer is always the local browser (`ws://127.0.0.1`), so this skips TLS and does not verify
//! `Sec-WebSocket-Accept`. Frames are written under a mutex so the reader thread and callers can share
//! one TCP connection; the reader owns its own clone of the stream.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

/// Largest message accepted from the browser. Tool lists with big schemas stay far below this.
pub const MAX_MESSAGE: usize = 64 << 20;

/// Splits `ws://host:port/path` into (`host:port`, `/path`).
pub fn parse_url(url: &str) -> io::Result<(String, String)> {
    let rest = url
        .strip_prefix("ws://")
        .ok_or_else(|| invalid(format!("only ws:// endpoints are supported: {url}")))?;
    let (addr, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if addr.is_empty() {
        return Err(invalid(format!("no host in {url}")));
    }
    Ok((addr.to_string(), path.to_string()))
}

fn invalid(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

/// The writing half: whole frames, one at a time.
pub struct Writer {
    stream: Mutex<TcpStream>,
    mask_seed: AtomicU32,
}

impl Writer {
    pub fn send_text(&self, text: &str) -> io::Result<()> {
        self.send(0x1, text.as_bytes())
    }

    fn send(&self, opcode: u8, payload: &[u8]) -> io::Result<()> {
        let frame = encode_frame(opcode, payload, self.next_mask());
        let mut stream = self.stream.lock().unwrap_or_else(|e| e.into_inner());
        stream.write_all(&frame)
    }

    /// Client frames must be masked; the key only has to vary, not be secret, on a loopback socket.
    fn next_mask(&self) -> [u8; 4] {
        let mut x = self.mask_seed.fetch_add(0x9E37_79B9, Ordering::Relaxed) | 1;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        x.to_le_bytes()
    }
}

/// The reading half, owned by one thread.
pub struct Reader {
    stream: BufReader<TcpStream>,
    writer: Arc<Writer>,
}

impl Reader {
    /// Next complete text message, or `None` once the browser closes the socket.
    pub fn next_text(&mut self) -> io::Result<Option<String>> {
        let mut message: Vec<u8> = Vec::new();
        loop {
            let Some((fin, opcode, payload)) = read_frame(&mut self.stream)? else {
                return Ok(None);
            };
            match opcode {
                0x0..=0x2 => {
                    if message.len() + payload.len() > MAX_MESSAGE {
                        return Err(invalid("message from the browser is too large".into()));
                    }
                    message.extend_from_slice(&payload);
                    if fin {
                        return String::from_utf8(message)
                            .map(Some)
                            .map_err(|_| invalid("message is not UTF-8".into()));
                    }
                }
                0x8 => return Ok(None),
                0x9 => self.writer.send(0xA, &payload)?,
                _ => {} // pong and reserved opcodes carry nothing for us
            }
        }
    }
}

/// Opens a WebSocket to `url` and returns its two halves.
pub fn connect(url: &str) -> io::Result<(Reader, Arc<Writer>)> {
    let (addr, path) = parse_url(url)?;
    let mut stream = TcpStream::connect(&addr)?;
    stream.set_nodelay(true)?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
    )?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut status = String::new();
    reader.read_line(&mut status)?;
    if !status.starts_with("HTTP/1.1 101") {
        return Err(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            format!(
                "the browser refused the DevTools connection ({}). Is remote debugging allowed?",
                status.trim()
            ),
        ));
    }
    // Skip the remaining response headers.
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 || line == "\r\n" {
            break;
        }
    }
    let writer = Arc::new(Writer {
        stream: Mutex::new(stream),
        mask_seed: AtomicU32::new(0x1234_5678),
    });
    Ok((
        Reader {
            stream: reader,
            writer: writer.clone(),
        },
        writer,
    ))
}

/// Builds one masked, final client frame.
pub fn encode_frame(opcode: u8, payload: &[u8], mask: [u8; 4]) -> Vec<u8> {
    let len = payload.len();
    let mut out = Vec::with_capacity(len + 14);
    out.push(0x80 | opcode);
    if len < 126 {
        out.push(0x80 | len as u8);
    } else if len <= u16::MAX as usize {
        out.push(0x80 | 126);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        out.push(0x80 | 127);
        out.extend_from_slice(&(len as u64).to_be_bytes());
    }
    out.extend_from_slice(&mask);
    out.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i & 3]));
    out
}

/// Reads one frame: (fin, opcode, unmasked payload). `None` at a clean end of stream.
pub fn read_frame(r: &mut impl Read) -> io::Result<Option<(bool, u8, Vec<u8>)>> {
    let mut head = [0u8; 2];
    match r.read_exact(&mut head) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let fin = head[0] & 0x80 != 0;
    let opcode = head[0] & 0x0F;
    let masked = head[1] & 0x80 != 0;
    let len = match head[1] & 0x7F {
        126 => {
            let mut b = [0u8; 2];
            r.read_exact(&mut b)?;
            u16::from_be_bytes(b) as u64
        }
        127 => {
            let mut b = [0u8; 8];
            r.read_exact(&mut b)?;
            u64::from_be_bytes(b)
        }
        n => n as u64,
    };
    if len > MAX_MESSAGE as u64 {
        return Err(invalid("frame from the browser is too large".into()));
    }
    let mut mask = [0u8; 4];
    if masked {
        r.read_exact(&mut mask)?;
    }
    let mut payload = vec![0u8; len as usize];
    r.read_exact(&mut payload)?;
    if masked {
        payload
            .iter_mut()
            .enumerate()
            .for_each(|(i, b)| *b ^= mask[i & 3]);
    }
    Ok(Some((fin, opcode, payload)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_at_every_length_class() {
        for len in [0usize, 5, 125, 126, 65_535, 65_536, 200_000] {
            let payload: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            let frame = encode_frame(0x1, &payload, [7, 1, 9, 3]);
            let (fin, op, got) = read_frame(&mut frame.as_slice()).unwrap().unwrap();
            assert!(fin);
            assert_eq!(op, 0x1);
            assert_eq!(got, payload, "length {len}");
        }
    }

    #[test]
    fn end_of_stream_is_none_and_oversize_is_rejected() {
        assert!(read_frame(&mut [].as_slice()).unwrap().is_none());
        let mut huge = vec![0x81, 127];
        huge.extend_from_slice(&(MAX_MESSAGE as u64 + 1).to_be_bytes());
        assert!(read_frame(&mut huge.as_slice()).is_err());
    }

    #[test]
    fn urls_split_into_address_and_path() {
        assert_eq!(
            parse_url("ws://127.0.0.1:9222/devtools/browser/abc").unwrap(),
            ("127.0.0.1:9222".into(), "/devtools/browser/abc".into())
        );
        assert!(parse_url("wss://x").is_err());
        assert!(parse_url("ws:///p").is_err());
    }
}
