use std::io::{self, Read};

use buffa::{Message, MessageView, bytes::Bytes};
use thiserror::Error;

use crate::proto::{
    BuildEvent, BuildEventId, BuildEventIdView, BuildEventOwnedView, BuildEventView,
};

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
    Decode(#[from] buffa::DecodeError),
}

/// A self-contained decoded BEP event whose generated view borrows from its
/// retained protobuf frame without copying strings or byte fields.
#[derive(Clone, Debug)]
pub struct BepEvent {
    inner: BuildEventOwnedView,
}

impl BepEvent {
    /// Decode and retain an already-owned protobuf frame without copying it.
    pub fn decode(frame: Vec<u8>) -> Result<Self, buffa::DecodeError> {
        Ok(Self {
            inner: BuildEventOwnedView::decode(Bytes::from(frame))?,
        })
    }

    /// Decode a borrowed frame by copying only the frame backing allocation.
    pub fn decode_slice(frame: &[u8]) -> Result<Self, buffa::DecodeError> {
        Ok(Self {
            inner: BuildEventOwnedView::decode(Bytes::copy_from_slice(frame))?,
        })
    }

    /// Encode an owned generated message and decode it as a retained view.
    pub fn from_owned(event: &BuildEvent) -> Result<Self, buffa::DecodeError> {
        Ok(Self {
            inner: BuildEventOwnedView::from_owned(event)?,
        })
    }

    #[must_use]
    pub fn view(&self) -> &BuildEventView<'_> {
        self.inner.view()
    }

    #[must_use]
    pub fn frame_bytes(&self) -> &[u8] {
        self.inner.bytes()
    }
}

#[derive(Debug)]
pub struct PartialStream {
    pub events: Vec<BepEvent>,
    pub terminal_error: Option<FrameError>,
}

#[derive(Debug)]
pub struct StreamOutcome {
    pub event_count: usize,
    pub decoded_bytes: usize,
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

pub fn decode_stream<R: Read>(reader: R, max_bytes: usize) -> Result<Vec<BepEvent>, FrameError> {
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
    reader: R,
    max_frame_bytes: usize,
    max_stream_bytes: usize,
    max_events: usize,
) -> PartialStream {
    let mut events = Vec::new();
    let outcome = visit_stream_partial_bounded(
        reader,
        max_frame_bytes,
        max_stream_bytes,
        max_events,
        |event| events.push(event),
    );
    PartialStream {
        events,
        terminal_error: outcome.terminal_error,
    }
}

/// Decode a length-delimited BEP stream one frame at a time without retaining
/// prior frames. The visitor owns each decoded event and decides what bounded
/// state, if any, should survive to the next event.
pub fn visit_stream_partial_bounded<R, F>(
    mut reader: R,
    max_frame_bytes: usize,
    max_stream_bytes: usize,
    max_events: usize,
    mut visitor: F,
) -> StreamOutcome
where
    R: Read,
    F: FnMut(BepEvent),
{
    let mut stream_bytes = 0_usize;
    let mut event_count = 0_usize;
    loop {
        match read_frame(&mut reader, max_frame_bytes) {
            Ok(Some(frame)) => {
                let next_bytes = stream_bytes.saturating_add(frame.len());
                if next_bytes > max_stream_bytes {
                    return StreamOutcome {
                        event_count,
                        decoded_bytes: stream_bytes,
                        terminal_error: Some(FrameError::StreamTooLarge {
                            actual: next_bytes,
                            limit: max_stream_bytes,
                        }),
                    };
                }
                if event_count >= max_events {
                    let actual = event_count.saturating_add(1);
                    return StreamOutcome {
                        event_count,
                        decoded_bytes: stream_bytes,
                        terminal_error: Some(FrameError::TooManyEvents {
                            actual,
                            limit: max_events,
                        }),
                    };
                }
                stream_bytes = next_bytes;
                match BepEvent::decode(frame) {
                    Ok(event) => {
                        visitor(event);
                        event_count = event_count.saturating_add(1);
                    }
                    Err(error) => {
                        return StreamOutcome {
                            event_count,
                            decoded_bytes: stream_bytes,
                            terminal_error: Some(FrameError::Decode(error)),
                        };
                    }
                }
            }
            Ok(None) => {
                return StreamOutcome {
                    event_count,
                    decoded_bytes: stream_bytes,
                    terminal_error: None,
                };
            }
            Err(error) => {
                return StreamOutcome {
                    event_count,
                    decoded_bytes: stream_bytes,
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

/// Decode a single borrowed frame into a retained zero-copy event handle.
pub fn decode_event(frame: &[u8]) -> Result<BepEvent, buffa::DecodeError> {
    BepEvent::decode_slice(frame)
}

/// Decode an event-id submessage as a borrowed view into its parent event.
pub fn decode_event_id(input: &[u8]) -> Result<BuildEventIdView<'_>, buffa::DecodeError> {
    BuildEventIdView::decode_view(input)
}

/// Encode an owned event-id submessage for fixtures and benchmarks.
#[must_use]
pub fn encode_event_id(id: &BuildEventId) -> Vec<u8> {
    id.encode_to_vec()
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
                    Box::new(crate::proto::Progress { stdout, stderr: String::new() })
                )),
                ..Default::default()
            };
            let framed = encode_frame(&event);
            let decoded = decode_stream(Cursor::new(framed), DEFAULT_MAX_FRAME_BYTES).unwrap();
            prop_assert_eq!(decoded.len(), 1);
            let Some(crate::view::build_event::Payload::Progress(progress)) =
                decoded[0].view().payload.as_ref()
            else {
                prop_assert!(false, "missing progress payload");
                return Ok(());
            };
            prop_assert_eq!(progress.stdout, event_progress_stdout(&event));
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
            payload: Some(crate::proto::build_event::Payload::Progress(Box::new(
                crate::proto::Progress {
                    stdout: "output".into(),
                    stderr: String::new(),
                },
            ))),
            ..Default::default()
        };
        let frame = encode_frame(&event);
        let mut input = frame.clone();
        input.extend_from_slice(&frame);
        let partial = decode_stream_partial_bounded(
            input.as_slice(),
            DEFAULT_MAX_FRAME_BYTES,
            event.encoded_len().saturating_sub(1) as usize,
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
        assert_eq!(partial.events.len(), 1);
        let Some(crate::view::build_event::Payload::Progress(progress)) =
            partial.events[0].view().payload.as_ref()
        else {
            panic!("missing progress payload");
        };
        assert_eq!(progress.stdout, "output");
        assert!(matches!(
            partial.terminal_error,
            Some(FrameError::TooManyEvents { .. })
        ));
    }

    #[test]
    fn ignores_unknown_fields_without_rejecting_the_event() {
        let event = BuildEvent::default();
        let mut body = event.encode_to_vec();
        body.extend_from_slice(&[0xf8, 0x07, 0x01]);
        let mut framed = Vec::new();
        write_varint(body.len() as u64, &mut framed);
        framed.extend_from_slice(&body);

        let decoded = decode_stream(framed.as_slice(), DEFAULT_MAX_FRAME_BYTES).unwrap();
        assert_eq!(decoded.len(), 1);
    }

    fn event_progress_stdout(event: &BuildEvent) -> &str {
        let Some(crate::proto::build_event::Payload::Progress(progress)) = event.payload.as_ref()
        else {
            panic!("missing progress payload");
        };
        &progress.stdout
    }
}
