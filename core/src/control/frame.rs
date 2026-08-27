//! Length-prefixed binary framing (`docs/core-protocol.md` "Framing").
//!
//! `u32 LE payload-length | u8 kind | payload`. PTY bytes ride their own
//! frame kinds so they never touch JSON (proposal KTD3) — pane-output
//! (server→client) and pane-input (client→server) both carry a `u64 LE`
//! pane id followed by the raw bytes.

use std::io::{self, Read, Write};

/// Refuse frames above this **before allocating** — the length is untrusted
/// numeric input (release builds wrap silently on overflow, and a buffer
/// sized from a peer's number is the `equal_reader` bug class).
pub const MAX_FRAME: u32 = 8 * 1024 * 1024;

const KIND_JSON: u8 = 0x01;
const KIND_PANE_OUTPUT: u8 = 0x02;
const KIND_PANE_INPUT: u8 = 0x03;

/// One wire frame, either direction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// A JSON envelope (request, response, or event).
    Json(Vec<u8>),
    /// Raw PTY output for a pane (server → client).
    PaneOutput { pane: u64, bytes: Vec<u8> },
    /// Raw keystroke bytes for a pane (client → server).
    PaneInput { pane: u64, bytes: Vec<u8> },
}

impl Frame {
    fn kind(&self) -> u8 {
        match self {
            Frame::Json(_) => KIND_JSON,
            Frame::PaneOutput { .. } => KIND_PANE_OUTPUT,
            Frame::PaneInput { .. } => KIND_PANE_INPUT,
        }
    }
}

/// Write one frame. The length prefix covers the payload only.
pub fn write_frame(w: &mut impl Write, frame: &Frame) -> io::Result<()> {
    let (pane_hdr, body): (Option<[u8; 8]>, &[u8]) = match frame {
        Frame::Json(b) => (None, b),
        Frame::PaneOutput { pane, bytes } | Frame::PaneInput { pane, bytes } => {
            (Some(pane.to_le_bytes()), bytes)
        }
    };
    let payload_len = body.len() + if pane_hdr.is_some() { 8 } else { 0 };
    let len = u32::try_from(payload_len)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "frame too large"))?;
    if len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "frame too large"));
    }
    w.write_all(&len.to_le_bytes())?;
    w.write_all(&[frame.kind()])?;
    if let Some(hdr) = pane_hdr {
        w.write_all(&hdr)?;
    }
    w.write_all(body)?;
    w.flush()
}

/// Read one frame. `Ok(None)` on clean EOF at a frame boundary; any error —
/// oversize length, unknown kind, short pane header, mid-frame EOF — is an
/// `Err`, and the caller must drop the connection (fail-closed on skew).
pub fn read_frame(r: &mut impl Read) -> io::Result<Option<Frame>> {
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len_buf);
    if len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut kind = [0u8; 1];
    r.read_exact(&mut kind)?;
    let mut payload = vec![0u8; len as usize];
    r.read_exact(&mut payload)?;
    match kind[0] {
        KIND_JSON => Ok(Some(Frame::Json(payload))),
        KIND_PANE_OUTPUT | KIND_PANE_INPUT => {
            if payload.len() < 8 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "short pane frame"));
            }
            let pane = u64::from_le_bytes(payload[..8].try_into().unwrap());
            let bytes = payload[8..].to_vec();
            Ok(Some(if kind[0] == KIND_PANE_OUTPUT {
                Frame::PaneOutput { pane, bytes }
            } else {
                Frame::PaneInput { pane, bytes }
            }))
        }
        _ => Err(io::Error::new(io::ErrorKind::InvalidData, "unknown frame kind")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(frame: Frame) -> Frame {
        let mut buf = Vec::new();
        write_frame(&mut buf, &frame).unwrap();
        read_frame(&mut &buf[..]).unwrap().unwrap()
    }

    #[test]
    fn json_roundtrips() {
        let f = Frame::Json(br#"{"id":1,"cmd":"core/ping"}"#.to_vec());
        assert_eq!(roundtrip(f.clone()), f);
    }

    #[test]
    fn pane_frames_roundtrip_binary_exact() {
        // Byte-exactness incl. NUL/ESC/high bytes is the point of KTD3.
        let bytes = vec![0x00, 0x1b, 0xff, b'x', 0x07];
        let f = Frame::PaneOutput { pane: u64::MAX, bytes: bytes.clone() };
        assert_eq!(roundtrip(f.clone()), f);
        let f = Frame::PaneInput { pane: 42, bytes };
        assert_eq!(roundtrip(f.clone()), f);
    }

    #[test]
    fn empty_payloads_roundtrip() {
        assert_eq!(roundtrip(Frame::Json(vec![])), Frame::Json(vec![]));
        let f = Frame::PaneInput { pane: 7, bytes: vec![] };
        assert_eq!(roundtrip(f.clone()), f);
    }

    #[test]
    fn clean_eof_is_none_mid_frame_eof_is_err() {
        assert!(read_frame(&mut &[][..]).unwrap().is_none());
        let mut buf = Vec::new();
        write_frame(&mut buf, &Frame::Json(b"{}".to_vec())).unwrap();
        buf.truncate(buf.len() - 1);
        assert!(read_frame(&mut &buf[..]).is_err());
    }

    #[test]
    fn oversize_length_refused_before_allocation() {
        let mut buf = (MAX_FRAME + 1).to_le_bytes().to_vec();
        buf.push(KIND_JSON);
        // No payload follows — if the reader tried to allocate/read it, this
        // would be UnexpectedEof; the guard must trip on the length alone.
        let err = read_frame(&mut &buf[..]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("too large"));
    }

    #[test]
    fn unknown_kind_is_err() {
        let mut buf = 0u32.to_le_bytes().to_vec();
        buf.push(0x7f);
        assert!(read_frame(&mut &buf[..]).is_err());
    }

    #[test]
    fn short_pane_payload_is_err() {
        let mut buf = 4u32.to_le_bytes().to_vec();
        buf.push(KIND_PANE_OUTPUT);
        buf.extend_from_slice(&[0, 0, 0, 0]);
        assert!(read_frame(&mut &buf[..]).is_err());
    }
}
