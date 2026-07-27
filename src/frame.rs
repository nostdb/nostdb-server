//! Message framing, as `server_protocol_version` 1 section 3 defines it.
//!
//! A message is a 4-byte unsigned big-endian byte count followed by exactly that many bytes of
//! UTF-8 JSON. Big-endian because it is the order the container header already uses, and a
//! second byte order in one product is a bug waiting for a different machine.
//!
//! One connection carries many messages in both directions. This is not
//! request-per-connection: a session spans messages, and reconnecting for each one would make a
//! transaction impossible to express.

use std::io::{self, Read, Write};

/// The smallest maximum frame size the contract allows.
///
/// Section 3 requires at least 8 MiB. A build may configure more; it may not configure less,
/// because a client that cannot send a legal message has no way to discover which side is wrong.
pub const MINIMUM_MAXIMUM_FRAME_BYTES: u32 = 8 * 1024 * 1024;

/// The length prefix's width on the wire.
const PREFIX_BYTES: usize = 4;

/// Why a frame was refused.
#[derive(Debug)]
pub enum FrameError {
    /// The declared length exceeds the configured maximum.
    ///
    /// This is section 8's `frame_too_large`. The refusal happens before the body is read, so a
    /// peer cannot name an allocation.
    TooLarge {
        /// The length the peer declared.
        declared: u32,
        /// The configured maximum.
        maximum: u32,
    },
    /// The body was not UTF-8.
    NotUtf8,
    /// The connection ended.
    ///
    /// A clean end between messages is not an error to the caller; it is reported so the caller
    /// can tell it apart from a truncated frame.
    Closed,
    /// The frame was cut short.
    Truncated,
    /// The underlying transport failed.
    Io(io::Error),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge { declared, maximum } => write!(
                formatter,
                "the peer declared a {declared}-byte frame, and the maximum is {maximum}"
            ),
            Self::NotUtf8 => formatter.write_str("a frame body must be UTF-8"),
            Self::Closed => formatter.write_str("the connection ended"),
            Self::Truncated => formatter.write_str("the frame was cut short"),
            Self::Io(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for FrameError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for FrameError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Reads one frame, refusing an over-long one before allocating for it.
///
/// The order here is the security property, not a style choice. The length is compared against
/// the maximum *before* any buffer is sized, so a peer that declares four gigabytes causes a
/// refusal rather than a four-gigabyte allocation. Sizing the buffer first and validating second
/// would hand an unauthenticated peer control of this process's memory.
///
/// # Errors
///
/// Returns [`FrameError::Closed`] when the connection ends cleanly between messages,
/// [`FrameError::Truncated`] when it ends inside one, [`FrameError::TooLarge`] when the declared
/// length exceeds `maximum`, and [`FrameError::NotUtf8`] when the body is not UTF-8.
pub fn read_frame<R: Read>(reader: &mut R, maximum: u32) -> Result<String, FrameError> {
    let mut prefix = [0_u8; PREFIX_BYTES];
    match read_exact_or_closed(reader, &mut prefix)? {
        Read0::Closed => return Err(FrameError::Closed),
        Read0::Filled => {}
    }

    let declared = u32::from_be_bytes(prefix);
    if declared > maximum {
        return Err(FrameError::TooLarge { declared, maximum });
    }

    // Only now is a buffer sized, and only to a length already known to be within the maximum.
    let mut body = vec![0_u8; declared as usize];
    reader.read_exact(&mut body).map_err(|error| {
        if error.kind() == io::ErrorKind::UnexpectedEof {
            FrameError::Truncated
        } else {
            FrameError::Io(error)
        }
    })?;

    String::from_utf8(body).map_err(|_| FrameError::NotUtf8)
}

/// Writes one frame.
///
/// # Errors
///
/// Returns [`FrameError::TooLarge`] when the payload exceeds `maximum`, so this process cannot
/// send a message its own peer must refuse, and [`FrameError::Io`] when the write fails.
pub fn write_frame<W: Write>(writer: &mut W, body: &str, maximum: u32) -> Result<(), FrameError> {
    let bytes = body.as_bytes();
    let declared = u32::try_from(bytes.len()).map_err(|_| FrameError::TooLarge {
        declared: u32::MAX,
        maximum,
    })?;
    if declared > maximum {
        return Err(FrameError::TooLarge { declared, maximum });
    }
    writer.write_all(&declared.to_be_bytes())?;
    writer.write_all(bytes)?;
    writer.flush()?;
    Ok(())
}

enum Read0 {
    Filled,
    Closed,
}

/// Fills `buffer`, distinguishing a clean end before any byte from a truncated read.
fn read_exact_or_closed<R: Read>(reader: &mut R, buffer: &mut [u8]) -> Result<Read0, FrameError> {
    let mut filled = 0;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..]) {
            Ok(0) if filled == 0 => return Ok(Read0::Closed),
            Ok(0) => return Err(FrameError::Truncated),
            Ok(count) => filled += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(FrameError::Io(error)),
        }
    }
    Ok(Read0::Filled)
}

#[cfg(test)]
mod tests {
    use super::{FrameError, MINIMUM_MAXIMUM_FRAME_BYTES, read_frame, write_frame};

    const MAX: u32 = MINIMUM_MAXIMUM_FRAME_BYTES;

    #[test]
    fn a_frame_round_trips() {
        let mut buffer = Vec::new();
        write_frame(&mut buffer, r#"{"message":"hello"}"#, MAX).expect("written");
        let mut cursor = std::io::Cursor::new(buffer);
        let body = read_frame(&mut cursor, MAX).expect("read");
        assert_eq!(body, r#"{"message":"hello"}"#);
    }

    #[test]
    fn two_frames_share_one_connection() {
        // Section 3 is explicit that this is not request-per-connection, because a session spans
        // messages.
        let mut buffer = Vec::new();
        write_frame(&mut buffer, "first", MAX).expect("written");
        write_frame(&mut buffer, "second", MAX).expect("written");
        let mut cursor = std::io::Cursor::new(buffer);
        assert_eq!(read_frame(&mut cursor, MAX).expect("first"), "first");
        assert_eq!(read_frame(&mut cursor, MAX).expect("second"), "second");
        assert!(matches!(
            read_frame(&mut cursor, MAX),
            Err(FrameError::Closed)
        ));
    }

    #[test]
    fn an_over_long_declared_length_is_refused_without_allocating_it() {
        // The frame declares one gibibyte and supplies no body at all. A reader that sized its
        // buffer before validating would try to allocate that much; this one refuses from the
        // prefix, which is why the test can pass four bytes and nothing else.
        let declared: u32 = 1024 * 1024 * 1024;
        let mut cursor = std::io::Cursor::new(declared.to_be_bytes().to_vec());
        match read_frame(&mut cursor, MAX) {
            Err(FrameError::TooLarge {
                declared: d,
                maximum,
            }) => {
                assert_eq!(d, declared);
                assert_eq!(maximum, MAX);
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_clean_end_between_messages_is_not_a_truncation() {
        let mut cursor = std::io::Cursor::new(Vec::new());
        assert!(matches!(
            read_frame(&mut cursor, MAX),
            Err(FrameError::Closed)
        ));
    }

    #[test]
    fn an_end_inside_a_frame_is_a_truncation() {
        // Four bytes of prefix declaring ten, and three bytes of body.
        let mut bytes = 10_u32.to_be_bytes().to_vec();
        bytes.extend_from_slice(b"abc");
        let mut cursor = std::io::Cursor::new(bytes);
        assert!(matches!(
            read_frame(&mut cursor, MAX),
            Err(FrameError::Truncated)
        ));
    }

    #[test]
    fn a_partial_length_prefix_is_a_truncation_rather_than_a_clean_end() {
        let mut cursor = std::io::Cursor::new(vec![0_u8, 0]);
        assert!(matches!(
            read_frame(&mut cursor, MAX),
            Err(FrameError::Truncated)
        ));
    }

    #[test]
    fn a_non_utf8_body_is_refused() {
        let mut bytes = 2_u32.to_be_bytes().to_vec();
        bytes.extend_from_slice(&[0xff, 0xfe]);
        let mut cursor = std::io::Cursor::new(bytes);
        assert!(matches!(
            read_frame(&mut cursor, MAX),
            Err(FrameError::NotUtf8)
        ));
    }

    #[test]
    fn a_writer_refuses_to_send_what_its_peer_would_refuse() {
        let mut buffer = Vec::new();
        let body = "x".repeat(64);
        match write_frame(&mut buffer, &body, 16) {
            Err(FrameError::TooLarge { declared, maximum }) => {
                assert_eq!(declared, 64);
                assert_eq!(maximum, 16);
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(buffer.is_empty(), "a refused write must send nothing");
    }

    #[test]
    fn the_length_is_big_endian_on_the_wire() {
        // Stated so a change of byte order is a failing test rather than a silent
        // incompatibility with a differently built peer.
        let mut buffer = Vec::new();
        write_frame(&mut buffer, "ab", MAX).expect("written");
        assert_eq!(&buffer[..4], &[0, 0, 0, 2]);
    }
}
