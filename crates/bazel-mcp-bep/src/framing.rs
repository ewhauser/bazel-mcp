use std::io::{self, Read};

use prost::Message;
use thiserror::Error;

use crate::proto::BuildEvent;

pub const DEFAULT_MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;
pub const DEFAULT_MAX_STREAM_BYTES: usize = 128 * 1024 * 1024;
pub const DEFAULT_MAX_STREAM_EVENTS: usize = 1_000_000;

#[derive(Debug, Error)]
pub enum FrameError {
    #[error("truncated varint-delimited BEP frame")]
    Truncated,
    #[error("invalid BEP frame length varint")]
    InvalidVarint,
    #[error("BEP frame length {actual} exceeds limit {limit}")]
    TooLarge { actual: usize, limit: usize },
    #[error("decoded BEP stream bytes {actual} exceed limit {limit}")]
    StreamTooLarge { actual: usize, limit: usize },
    #[error("decoded BEP event count {actual} exceeds limit {limit}")]
    TooManyEvents { actual: usize, limit: usize },
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Decode(#[from] prost::DecodeError),
}

#[derive(Debug)]
pub struct PartialStream {
    pub events: Vec<BuildEvent>,
    pub terminal_error: Option<FrameError>,
}

pub fn read_frame<R: Read>(
    reader: &mut R,
    max_bytes: usize,
) -> Result<Option<Vec<u8>>, FrameError> {
    let Some(length) = read_varint(reader)? else {
        return Ok(None);
    };
    let length = usize::try_from(length).map_err(|_| FrameError::TooLarge {
        actual: usize::MAX,
        limit: max_bytes,
    })?;
    if length > max_bytes {
        return Err(FrameError::TooLarge {
            actual: length,
            limit: max_bytes,
        });
    }
    let mut frame = vec![0_u8; length];
    reader.read_exact(&mut frame).map_err(|error| {
        if error.kind() == io::ErrorKind::UnexpectedEof {
            FrameError::Truncated
        } else {
            FrameError::Io(error)
        }
    })?;
    Ok(Some(frame))
}

pub fn decode_stream<R: Read>(reader: R, max_bytes: usize) -> Result<Vec<BuildEvent>, FrameError> {
    let partial = decode_stream_partial_bounded(
        reader,
        max_bytes,
        DEFAULT_MAX_STREAM_BYTES,
        DEFAULT_MAX_STREAM_EVENTS,
    );
    if let Some(error) = partial.terminal_error {
        Err(error)
    } else {
        Ok(partial.events)
    }
}

pub fn decode_stream_partial<R: Read>(reader: R, max_bytes: usize) -> PartialStream {
    decode_stream_partial_bounded(
        reader,
        max_bytes,
        DEFAULT_MAX_STREAM_BYTES,
        DEFAULT_MAX_STREAM_EVENTS,
    )
}

pub fn decode_stream_partial_bounded<R: Read>(
    mut reader: R,
    max_frame_bytes: usize,
    max_stream_bytes: usize,
    max_events: usize,
) -> PartialStream {
    let mut events = Vec::new();
    let mut stream_bytes = 0_usize;
    loop {
        match read_frame(&mut reader, max_frame_bytes) {
            Ok(Some(frame)) => {
                let next_bytes = stream_bytes.saturating_add(frame.len());
                if next_bytes > max_stream_bytes {
                    return PartialStream {
                        events,
                        terminal_error: Some(FrameError::StreamTooLarge {
                            actual: next_bytes,
                            limit: max_stream_bytes,
                        }),
                    };
                }
                if events.len() >= max_events {
                    let actual = events.len().saturating_add(1);
                    return PartialStream {
                        events,
                        terminal_error: Some(FrameError::TooManyEvents {
                            actual,
                            limit: max_events,
                        }),
                    };
                }
                stream_bytes = next_bytes;
                match BuildEvent::decode(frame.as_slice()) {
                    Ok(event) => events.push(event),
                    Err(error) => {
                        return PartialStream {
                            events,
                            terminal_error: Some(FrameError::Decode(error)),
                        };
                    }
                }
            }
            Ok(None) => {
                return PartialStream {
                    events,
                    terminal_error: None,
                };
            }
            Err(error) => {
                return PartialStream {
                    events,
                    terminal_error: Some(error),
                };
            }
        }
    }
}

#[must_use]
pub fn encode_frame<M: Message>(message: &M) -> Vec<u8> {
    let body = message.encode_to_vec();
    let mut framed = Vec::with_capacity(body.len() + 10);
    write_varint(body.len() as u64, &mut framed);
    framed.extend_from_slice(&body);
    framed
}

fn read_varint<R: Read>(reader: &mut R) -> Result<Option<u64>, FrameError> {
    let mut value = 0_u64;
    for shift in (0..70).step_by(7) {
        let mut byte = [0_u8; 1];
        match reader.read_exact(&mut byte) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof && shift == 0 => {
                return Ok(None);
            }
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                return Err(FrameError::Truncated);
            }
            Err(error) => return Err(FrameError::Io(error)),
        }
        let payload = u64::from(byte[0] & 0x7f);
        if shift == 63 && payload > 1 {
            return Err(FrameError::InvalidVarint);
        }
        value |= payload << shift;
        if byte[0] & 0x80 == 0 {
            return Ok(Some(value));
        }
    }
    Err(FrameError::InvalidVarint)
}

fn write_varint(mut value: u64, output: &mut Vec<u8>) {
    while value >= 0x80 {
        output.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    output.push(value as u8);
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use proptest::prelude::*;

    use super::*;

    proptest! {
        #[test]
        fn frame_round_trips(stdout in ".{0,1024}") {
            let event = BuildEvent {
                payload: Some(crate::proto::build_event::Payload::Progress(
                    crate::proto::Progress { stdout, stderr: String::new() }
                )),
                ..Default::default()
            };
            let framed = encode_frame(&event);
            let decoded = decode_stream(Cursor::new(framed), DEFAULT_MAX_FRAME_BYTES).unwrap();
            prop_assert_eq!(decoded, vec![event]);
        }
    }

    #[test]
    fn rejects_truncated_frames() {
        let input = [4_u8, 1, 2];
        assert!(matches!(
            read_frame(&mut input.as_slice(), 1024),
            Err(FrameError::Truncated)
        ));
    }

    #[test]
    fn rejects_oversized_frames_before_allocating() {
        let input = [0xff_u8, 0x7f];
        assert!(matches!(
            read_frame(&mut input.as_slice(), 10),
            Err(FrameError::TooLarge { .. })
        ));
    }

    #[test]
    fn retains_partial_events_when_the_stream_budget_is_exhausted() {
        let event = BuildEvent {
            payload: Some(crate::proto::build_event::Payload::Progress(
                crate::proto::Progress {
                    stdout: "output".into(),
                    stderr: String::new(),
                },
            )),
            ..Default::default()
        };
        let frame = encode_frame(&event);
        let mut input = frame.clone();
        input.extend_from_slice(&frame);
        let partial = decode_stream_partial_bounded(
            input.as_slice(),
            DEFAULT_MAX_FRAME_BYTES,
            event.encoded_len().saturating_sub(1),
            10,
        );
        assert!(partial.events.is_empty());
        assert!(matches!(
            partial.terminal_error,
            Some(FrameError::StreamTooLarge { .. })
        ));

        let partial = decode_stream_partial_bounded(
            input.as_slice(),
            DEFAULT_MAX_FRAME_BYTES,
            DEFAULT_MAX_STREAM_BYTES,
            1,
        );
        assert_eq!(partial.events, vec![event]);
        assert!(matches!(
            partial.terminal_error,
            Some(FrameError::TooManyEvents { .. })
        ));
    }
}
