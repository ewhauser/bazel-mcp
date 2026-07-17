use std::io::{self, Read};

use buffa::{Message, MessageView, bytes::Bytes};
use thiserror::Error;

use crate::proto::{
    BuildEvent, BuildEventId, BuildEventIdView, BuildEventOwnedView, BuildEventView,
};

pub const DEFAULT_MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;
pub const DEFAULT_MAX_STREAM_BYTES: usize = 128 * 1024 * 1024;
pub const DEFAULT_MAX_STREAM_EVENTS: usize = 1_000_000;
const MAX_RETAINED_PENDING_BYTES: usize = 64 * 1024;

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

/// A decoded BEP event that borrows its protobuf frame.
///
/// Borrowed events are valid only for the duration of the visitor call that
/// receives them. Consumers that retain an event beyond that call should use
/// [`BepEvent`] instead.
#[derive(Clone, Debug)]
pub struct BorrowedBepEvent<'a> {
    inner: BuildEventView<'a>,
    frame: &'a [u8],
}

impl<'a> BorrowedBepEvent<'a> {
    fn decode(frame: &'a [u8]) -> Result<Self, buffa::DecodeError> {
        Ok(Self {
            inner: BuildEventView::decode_view(frame)?,
            frame,
        })
    }

    #[must_use]
    pub fn view(&self) -> &BuildEventView<'_> {
        &self.inner
    }

    #[must_use]
    pub fn frame_bytes(&self) -> &'a [u8] {
        self.frame
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

/// Controls how incremental stream accounting proceeds after a visited frame.
///
/// Bazel can close and reopen a BEP transport when it retries an invocation.
/// Callers that recognize the terminal event for an abandoned attempt can
/// reset the byte and event limits before the next frame without losing exact
/// frame boundaries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IncrementalStreamControl {
    Continue,
    ResetAfterFrame,
}

/// Incrementally decodes a varint-delimited BEP stream across arbitrary input
/// chunk boundaries.
///
/// At most one incomplete frame is retained between calls to [`Self::push`].
/// Complete frames are decoded and passed to the visitor immediately, so the
/// caller can reduce a BEP file while Bazel is still appending to it.
pub struct IncrementalStreamDecoder {
    pending: Vec<u8>,
    decoded_bytes: usize,
    event_count: usize,
    max_frame_bytes: usize,
    max_stream_bytes: usize,
    max_events: usize,
    terminal_error: Option<FrameError>,
}

impl IncrementalStreamDecoder {
    #[must_use]
    pub fn new(max_frame_bytes: usize, max_stream_bytes: usize, max_events: usize) -> Self {
        Self {
            pending: Vec::new(),
            decoded_bytes: 0,
            event_count: 0,
            max_frame_bytes,
            max_stream_bytes,
            max_events,
            terminal_error: None,
        }
    }

    /// Consume a stream chunk and visit every complete event it contains.
    ///
    /// Once a terminal framing, limit, or decode error is encountered, later
    /// chunks are ignored and the complete prefix remains available from
    /// [`Self::finish`].
    pub fn push<F>(&mut self, input: &[u8], mut visitor: F)
    where
        F: FnMut(BepEvent),
    {
        self.push_framed(input, |event, _frame| {
            visitor(event);
            IncrementalStreamControl::Continue
        });
    }

    /// Consume a stream chunk while exposing each exact length-delimited frame.
    ///
    /// The frame slice includes the original varint prefix. Returning
    /// [`IncrementalStreamControl::ResetAfterFrame`] resets stream byte/event
    /// accounting after that frame, which lets retry-aware transports retain
    /// only the next attempt even when two writers' bytes arrive in one read.
    pub fn push_framed<F>(&mut self, input: &[u8], mut visitor: F)
    where
        F: FnMut(BepEvent, &[u8]) -> IncrementalStreamControl,
    {
        self.push_frames(input, |decoder, frame, framed| {
            decoder.decode_owned_frame(frame, framed, &mut visitor);
        });
    }

    /// Consume a stream chunk and visit every complete event as a borrowed
    /// view into the caller's chunk or the decoder's one pending frame.
    ///
    /// The higher-ranked visitor prevents the borrowed event from escaping the
    /// callback, so the decoder can safely reuse its input storage afterward.
    pub fn push_borrowed<F>(&mut self, input: &[u8], mut visitor: F)
    where
        F: for<'frame> FnMut(BorrowedBepEvent<'frame>),
    {
        self.push_framed_borrowed(input, |event, _frame| {
            visitor(event);
            IncrementalStreamControl::Continue
        });
    }

    /// Consume a stream chunk while exposing a borrowed event and its exact
    /// original length-delimited frame.
    pub fn push_framed_borrowed<F>(&mut self, input: &[u8], mut visitor: F)
    where
        F: for<'frame> FnMut(BorrowedBepEvent<'frame>, &'frame [u8]) -> IncrementalStreamControl,
    {
        self.push_frames(input, |decoder, frame, framed| {
            decoder.decode_borrowed_frame(frame, framed, &mut visitor);
        });
    }

    fn push_frames<F>(&mut self, mut input: &[u8], mut decode: F)
    where
        F: for<'frame> FnMut(&mut Self, &'frame [u8], &'frame [u8]),
    {
        if self.terminal_error.is_some() {
            return;
        }

        while !input.is_empty() {
            if self.pending.is_empty() {
                match inspect_frame(input, self.max_frame_bytes) {
                    Ok(FrameState::Complete {
                        prefix_bytes,
                        frame_bytes,
                    }) => {
                        let consumed = prefix_bytes.saturating_add(frame_bytes);
                        decode(self, &input[prefix_bytes..consumed], &input[..consumed]);
                        if self.terminal_error.is_some() {
                            return;
                        }
                        input = &input[consumed..];
                    }
                    Ok(FrameState::NeedPrefix) => {
                        self.pending.extend_from_slice(input);
                        return;
                    }
                    Ok(FrameState::NeedPayload { total_bytes }) => {
                        debug_assert!(input.len() < total_bytes);
                        self.pending.extend_from_slice(input);
                        return;
                    }
                    Err(error) => {
                        self.terminal_error = Some(error);
                        return;
                    }
                }
                continue;
            }

            match inspect_frame(&self.pending, self.max_frame_bytes) {
                Ok(FrameState::NeedPrefix) => {
                    // Stop at the first terminating varint byte. Appending a
                    // wider slice here could also consume bytes belonging to
                    // this frame's payload or the next frame.
                    self.pending.push(input[0]);
                    input = &input[1..];
                }
                Ok(FrameState::NeedPayload { total_bytes }) => {
                    let take = input
                        .len()
                        .min(total_bytes.saturating_sub(self.pending.len()));
                    self.pending.extend_from_slice(&input[..take]);
                    input = &input[take..];
                }
                Ok(FrameState::Complete {
                    prefix_bytes,
                    frame_bytes,
                }) => {
                    let consumed = prefix_bytes.saturating_add(frame_bytes);
                    let framed = std::mem::take(&mut self.pending);
                    decode(self, &framed[prefix_bytes..consumed], &framed[..consumed]);
                    self.recycle_pending(framed);
                    if self.terminal_error.is_some() {
                        return;
                    }
                }
                Err(error) => {
                    self.terminal_error = Some(error);
                    return;
                }
            }
        }

        // A chunk can provide exactly the bytes needed to complete the one
        // retained frame. Decode it now instead of waiting for another push or
        // incorrectly reporting it as truncated from `finish`.
        if !self.pending.is_empty() {
            match inspect_frame(&self.pending, self.max_frame_bytes) {
                Ok(FrameState::Complete {
                    prefix_bytes,
                    frame_bytes,
                }) => {
                    let consumed = prefix_bytes.saturating_add(frame_bytes);
                    let framed = std::mem::take(&mut self.pending);
                    decode(self, &framed[prefix_bytes..consumed], &framed[..consumed]);
                    self.recycle_pending(framed);
                }
                Ok(FrameState::NeedPrefix | FrameState::NeedPayload { .. }) => {}
                Err(error) => self.terminal_error = Some(error),
            }
        }
    }

    #[must_use]
    pub fn is_terminal(&self) -> bool {
        self.terminal_error.is_some()
    }

    /// Finish the stream, treating a retained prefix or payload as truncated.
    #[must_use]
    pub fn finish(mut self) -> StreamOutcome {
        if self.terminal_error.is_none() && !self.pending.is_empty() {
            self.terminal_error = Some(FrameError::Truncated);
        }
        StreamOutcome {
            event_count: self.event_count,
            decoded_bytes: self.decoded_bytes,
            terminal_error: self.terminal_error,
        }
    }

    fn recycle_pending(&mut self, mut framed: Vec<u8>) {
        if framed.capacity() <= MAX_RETAINED_PENDING_BYTES {
            framed.clear();
            self.pending = framed;
        }
    }

    fn decode_owned_frame<F>(&mut self, frame: &[u8], framed: &[u8], visitor: &mut F)
    where
        F: FnMut(BepEvent, &[u8]) -> IncrementalStreamControl,
    {
        self.visit_decoded_frame(frame.len(), || {
            let event = BepEvent::decode_slice(frame)?;
            Ok(visitor(event, framed))
        });
    }

    fn decode_borrowed_frame<'frame, F>(
        &mut self,
        frame: &'frame [u8],
        framed: &'frame [u8],
        visitor: &mut F,
    ) where
        F: for<'a> FnMut(BorrowedBepEvent<'a>, &'a [u8]) -> IncrementalStreamControl,
    {
        self.visit_decoded_frame(frame.len(), || {
            let event = BorrowedBepEvent::decode(frame)?;
            Ok(visitor(event, framed))
        });
    }

    fn visit_decoded_frame<F>(&mut self, frame_bytes: usize, decode: F)
    where
        F: FnOnce() -> Result<IncrementalStreamControl, buffa::DecodeError>,
    {
        let next_bytes = self.decoded_bytes.saturating_add(frame_bytes);
        if next_bytes > self.max_stream_bytes {
            self.terminal_error = Some(FrameError::StreamTooLarge {
                actual: next_bytes,
                limit: self.max_stream_bytes,
            });
            return;
        }
        if self.event_count >= self.max_events {
            self.terminal_error = Some(FrameError::TooManyEvents {
                actual: self.event_count.saturating_add(1),
                limit: self.max_events,
            });
            return;
        }
        self.decoded_bytes = next_bytes;
        match decode() {
            Ok(control) => match control {
                IncrementalStreamControl::Continue => {
                    self.event_count = self.event_count.saturating_add(1);
                }
                IncrementalStreamControl::ResetAfterFrame => {
                    self.decoded_bytes = 0;
                    self.event_count = 0;
                }
            },
            Err(error) => self.terminal_error = Some(FrameError::Decode(error)),
        }
    }

    fn fail(&mut self, error: FrameError) {
        if self.terminal_error.is_none() {
            self.terminal_error = Some(error);
        }
    }
}

enum FrameState {
    NeedPrefix,
    NeedPayload {
        total_bytes: usize,
    },
    Complete {
        prefix_bytes: usize,
        frame_bytes: usize,
    },
}

fn inspect_frame(input: &[u8], max_bytes: usize) -> Result<FrameState, FrameError> {
    let mut value = 0_u64;
    for (index, byte) in input.iter().copied().take(10).enumerate() {
        let shift = index * 7;
        let payload = u64::from(byte & 0x7f);
        if shift == 63 && payload > 1 {
            return Err(FrameError::InvalidVarint);
        }
        value |= payload << shift;
        if byte & 0x80 == 0 {
            let frame_bytes = usize::try_from(value).map_err(|_| FrameError::TooLarge {
                actual: usize::MAX,
                limit: max_bytes,
            })?;
            if frame_bytes > max_bytes {
                return Err(FrameError::TooLarge {
                    actual: frame_bytes,
                    limit: max_bytes,
                });
            }
            let prefix_bytes = index + 1;
            let total_bytes = prefix_bytes.saturating_add(frame_bytes);
            return if input.len() >= total_bytes {
                Ok(FrameState::Complete {
                    prefix_bytes,
                    frame_bytes,
                })
            } else {
                Ok(FrameState::NeedPayload { total_bytes })
            };
        }
    }
    if input.len() >= 10 {
        Err(FrameError::InvalidVarint)
    } else {
        Ok(FrameState::NeedPrefix)
    }
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
    let mut decoder = IncrementalStreamDecoder::new(max_frame_bytes, max_stream_bytes, max_events);
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                decoder.push(&buffer[..read], &mut visitor);
                if decoder.is_terminal() {
                    break;
                }
            }
            Err(error) => {
                decoder.fail(FrameError::Io(error));
                break;
            }
        }
    }
    decoder.finish()
}

/// Decode a length-delimited BEP stream one frame at a time while borrowing
/// each decoded event from the reusable read buffer.
///
/// The visitor must reduce or copy any state it needs before returning. This
/// avoids allocating a retained frame for consumers such as
/// `BepAccumulator` that already keep only bounded, reducer-relevant fields.
pub fn visit_stream_partial_borrowed_bounded<R, F>(
    mut reader: R,
    max_frame_bytes: usize,
    max_stream_bytes: usize,
    max_events: usize,
    mut visitor: F,
) -> StreamOutcome
where
    R: Read,
    F: for<'frame> FnMut(BorrowedBepEvent<'frame>),
{
    let mut decoder = IncrementalStreamDecoder::new(max_frame_bytes, max_stream_bytes, max_events);
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                decoder.push_borrowed(&buffer[..read], &mut visitor);
                if decoder.is_terminal() {
                    break;
                }
            }
            Err(error) => {
                decoder.fail(FrameError::Io(error));
                break;
            }
        }
    }
    decoder.finish()
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

    #[test]
    fn incremental_decoder_handles_every_chunk_boundary() {
        let first = BuildEvent {
            payload: Some(crate::proto::build_event::Payload::Progress(Box::new(
                crate::proto::Progress {
                    stdout: "x".repeat(256),
                    stderr: String::new(),
                },
            ))),
            ..Default::default()
        };
        let second = BuildEvent::default();
        let mut framed = encode_frame(&first);
        framed.extend_from_slice(&encode_frame(&second));

        for split in 0..=framed.len() {
            let mut decoder = IncrementalStreamDecoder::new(
                DEFAULT_MAX_FRAME_BYTES,
                DEFAULT_MAX_STREAM_BYTES,
                DEFAULT_MAX_STREAM_EVENTS,
            );
            let mut events = Vec::new();
            decoder.push(&framed[..split], |event| events.push(event));
            decoder.push(&framed[split..], |event| events.push(event));
            let outcome = decoder.finish();
            assert_eq!(outcome.event_count, 2, "split at byte {split}");
            assert_eq!(
                outcome.decoded_bytes,
                first.encoded_len() as usize + second.encoded_len() as usize,
                "split at byte {split}"
            );
            assert!(
                outcome.terminal_error.is_none(),
                "split at byte {split}: {:?}",
                outcome.terminal_error
            );
            assert_eq!(events.len(), 2, "split at byte {split}");
        }
    }

    #[test]
    fn incremental_decoder_reuses_pending_frame_capacity() {
        let event = BuildEvent {
            payload: Some(crate::proto::build_event::Payload::Progress(Box::new(
                crate::proto::Progress {
                    stdout: "retained".repeat(512),
                    stderr: String::new(),
                },
            ))),
            ..Default::default()
        };
        let framed = encode_frame(&event);
        let split = framed.len() / 2;
        let mut decoder = IncrementalStreamDecoder::new(
            DEFAULT_MAX_FRAME_BYTES,
            DEFAULT_MAX_STREAM_BYTES,
            DEFAULT_MAX_STREAM_EVENTS,
        );
        let mut events = 0;

        decoder.push_borrowed(&framed[..split], |_| events += 1);
        decoder.push_borrowed(&framed[split..], |_| events += 1);
        let retained_capacity = decoder.pending.capacity();
        assert!(decoder.pending.is_empty());
        assert!(retained_capacity >= framed.len());

        decoder.push_borrowed(&framed[..split], |_| events += 1);
        assert_eq!(decoder.pending.capacity(), retained_capacity);
        decoder.push_borrowed(&framed[split..], |_| events += 1);

        let outcome = decoder.finish();
        assert_eq!(events, 2);
        assert_eq!(outcome.event_count, 2);
        assert!(outcome.terminal_error.is_none());
    }

    #[test]
    fn incremental_decoder_drops_oversized_pending_capacity() {
        let event = BuildEvent {
            payload: Some(crate::proto::build_event::Payload::Progress(Box::new(
                crate::proto::Progress {
                    stdout: "x".repeat(MAX_RETAINED_PENDING_BYTES * 2),
                    stderr: String::new(),
                },
            ))),
            ..Default::default()
        };
        let framed = encode_frame(&event);
        let split = framed.len() / 2;
        let mut decoder = IncrementalStreamDecoder::new(
            DEFAULT_MAX_FRAME_BYTES,
            DEFAULT_MAX_STREAM_BYTES,
            DEFAULT_MAX_STREAM_EVENTS,
        );

        decoder.push_borrowed(&framed[..split], |_| {});
        decoder.push_borrowed(&framed[split..], |_| {});

        assert!(decoder.pending.is_empty());
        assert_eq!(decoder.pending.capacity(), 0);
        assert!(decoder.finish().terminal_error.is_none());
    }

    #[test]
    fn borrowed_incremental_decoder_handles_every_chunk_boundary() {
        let first = BuildEvent {
            payload: Some(crate::proto::build_event::Payload::Progress(Box::new(
                crate::proto::Progress {
                    stdout: "borrowed".repeat(32),
                    stderr: String::new(),
                },
            ))),
            ..Default::default()
        };
        let second = BuildEvent::default();
        let mut framed = encode_frame(&first);
        framed.extend_from_slice(&encode_frame(&second));

        for split in 0..=framed.len() {
            let mut decoder = IncrementalStreamDecoder::new(
                DEFAULT_MAX_FRAME_BYTES,
                DEFAULT_MAX_STREAM_BYTES,
                DEFAULT_MAX_STREAM_EVENTS,
            );
            let mut payloads = Vec::new();
            for chunk in [&framed[..split], &framed[split..]] {
                decoder.push_borrowed(chunk, |event| {
                    let stdout = match event.view().payload.as_ref() {
                        Some(crate::view::build_event::Payload::Progress(progress)) => {
                            Some(progress.stdout.to_owned())
                        }
                        _ => None,
                    };
                    payloads.push((event.frame_bytes().len(), stdout));
                });
            }
            let outcome = decoder.finish();
            assert_eq!(outcome.event_count, 2, "split at byte {split}");
            assert_eq!(
                outcome.decoded_bytes,
                first.encoded_len() as usize + second.encoded_len() as usize,
                "split at byte {split}"
            );
            assert!(
                outcome.terminal_error.is_none(),
                "split at byte {split}: {:?}",
                outcome.terminal_error
            );
            assert_eq!(
                payloads,
                [
                    (first.encoded_len() as usize, Some("borrowed".repeat(32))),
                    (second.encoded_len() as usize, None),
                ],
                "split at byte {split}"
            );
        }
    }

    #[test]
    fn framed_incremental_decoder_preserves_bytes_and_resets_between_attempts() {
        let first = BuildEvent {
            payload: Some(crate::proto::build_event::Payload::Progress(Box::new(
                crate::proto::Progress {
                    stdout: "abandoned".into(),
                    stderr: String::new(),
                },
            ))),
            ..Default::default()
        };
        let second = BuildEvent {
            payload: Some(crate::proto::build_event::Payload::Progress(Box::new(
                crate::proto::Progress {
                    stdout: "retained".into(),
                    stderr: String::new(),
                },
            ))),
            ..Default::default()
        };
        let first_frame = encode_frame(&first);
        let second_frame = encode_frame(&second);
        let mut input = first_frame.clone();
        input.extend_from_slice(&second_frame);

        for split in 0..=input.len() {
            let mut decoder = IncrementalStreamDecoder::new(
                DEFAULT_MAX_FRAME_BYTES,
                DEFAULT_MAX_STREAM_BYTES,
                DEFAULT_MAX_STREAM_EVENTS,
            );
            let mut frames = Vec::new();
            let mut ordinal = 0;
            for chunk in [&input[..split], &input[split..]] {
                decoder.push_framed(chunk, |_event, framed| {
                    frames.push(framed.to_vec());
                    ordinal += 1;
                    if ordinal == 1 {
                        IncrementalStreamControl::ResetAfterFrame
                    } else {
                        IncrementalStreamControl::Continue
                    }
                });
            }
            let outcome = decoder.finish();
            assert_eq!(frames, [first_frame.clone(), second_frame.clone()]);
            assert_eq!(outcome.event_count, 1, "split at byte {split}");
            assert_eq!(
                outcome.decoded_bytes,
                second.encoded_len() as usize,
                "split at byte {split}"
            );
            assert!(outcome.terminal_error.is_none(), "split at byte {split}");
        }
    }

    #[test]
    fn borrowed_framed_decoder_preserves_bytes_and_resets_between_attempts() {
        let first = BuildEvent {
            payload: Some(crate::proto::build_event::Payload::Progress(Box::new(
                crate::proto::Progress {
                    stdout: "abandoned".into(),
                    stderr: String::new(),
                },
            ))),
            ..Default::default()
        };
        let second = BuildEvent {
            payload: Some(crate::proto::build_event::Payload::Progress(Box::new(
                crate::proto::Progress {
                    stdout: "retained".into(),
                    stderr: String::new(),
                },
            ))),
            ..Default::default()
        };
        let first_frame = encode_frame(&first);
        let second_frame = encode_frame(&second);
        let mut input = first_frame.clone();
        input.extend_from_slice(&second_frame);

        for split in 0..=input.len() {
            let mut decoder = IncrementalStreamDecoder::new(
                DEFAULT_MAX_FRAME_BYTES,
                DEFAULT_MAX_STREAM_BYTES,
                DEFAULT_MAX_STREAM_EVENTS,
            );
            let mut frames = Vec::new();
            for chunk in [&input[..split], &input[split..]] {
                decoder.push_framed_borrowed(chunk, |event, framed| {
                    frames.push((event.frame_bytes().to_vec(), framed.to_vec()));
                    if frames.len() == 1 {
                        IncrementalStreamControl::ResetAfterFrame
                    } else {
                        IncrementalStreamControl::Continue
                    }
                });
            }
            let outcome = decoder.finish();
            assert_eq!(
                frames,
                [
                    (first.encode_to_vec(), first_frame.clone()),
                    (second.encode_to_vec(), second_frame.clone()),
                ],
                "split at byte {split}"
            );
            assert_eq!(outcome.event_count, 1, "split at byte {split}");
            assert_eq!(
                outcome.decoded_bytes,
                second.encoded_len() as usize,
                "split at byte {split}"
            );
            assert!(outcome.terminal_error.is_none(), "split at byte {split}");
        }
    }

    #[test]
    fn incremental_decoder_retains_complete_prefix_before_truncation() {
        let event = BuildEvent {
            payload: Some(crate::proto::build_event::Payload::Progress(Box::new(
                crate::proto::Progress {
                    stdout: "partial".into(),
                    stderr: String::new(),
                },
            ))),
            ..Default::default()
        };
        let frame = encode_frame(&event);
        let mut input = frame.clone();
        input.extend_from_slice(&frame[..frame.len().saturating_sub(1)]);

        let mut decoder = IncrementalStreamDecoder::new(
            DEFAULT_MAX_FRAME_BYTES,
            DEFAULT_MAX_STREAM_BYTES,
            DEFAULT_MAX_STREAM_EVENTS,
        );
        let mut events = Vec::new();
        for byte in input {
            decoder.push(&[byte], |event| events.push(event));
        }
        let outcome = decoder.finish();
        assert_eq!(events.len(), 1);
        assert_eq!(outcome.event_count, 1);
        assert!(matches!(
            outcome.terminal_error,
            Some(FrameError::Truncated)
        ));
    }

    #[test]
    fn incremental_decoder_rejects_invalid_and_oversized_prefixes_early() {
        let mut invalid = IncrementalStreamDecoder::new(1024, 4096, 10);
        invalid.push(&[0x80; 9], |_| {});
        assert!(!invalid.is_terminal());
        invalid.push(&[0x80], |_| {});
        assert!(matches!(
            invalid.finish().terminal_error,
            Some(FrameError::InvalidVarint)
        ));

        let mut oversized = IncrementalStreamDecoder::new(10, 4096, 10);
        oversized.push(&[0xff], |_| {});
        assert!(!oversized.is_terminal());
        oversized.push(&[0x7f], |_| {});
        assert!(matches!(
            oversized.finish().terminal_error,
            Some(FrameError::TooLarge { .. })
        ));
    }

    #[test]
    fn incremental_and_reader_paths_report_identical_limits() {
        let event = BuildEvent {
            payload: Some(crate::proto::build_event::Payload::Progress(Box::new(
                crate::proto::Progress {
                    stdout: "bounded".into(),
                    stderr: String::new(),
                },
            ))),
            ..Default::default()
        };
        let frame = encode_frame(&event);
        let mut input = frame.clone();
        input.extend_from_slice(&frame);

        let reader_outcome = visit_stream_partial_bounded(
            input.as_slice(),
            DEFAULT_MAX_FRAME_BYTES,
            DEFAULT_MAX_STREAM_BYTES,
            1,
            |_| {},
        );
        let borrowed_reader_outcome = visit_stream_partial_borrowed_bounded(
            input.as_slice(),
            DEFAULT_MAX_FRAME_BYTES,
            DEFAULT_MAX_STREAM_BYTES,
            1,
            |_| {},
        );
        let mut incremental =
            IncrementalStreamDecoder::new(DEFAULT_MAX_FRAME_BYTES, DEFAULT_MAX_STREAM_BYTES, 1);
        incremental.push(&input[..1], |_| {});
        incremental.push(&input[1..], |_| {});
        let incremental_outcome = incremental.finish();

        assert_eq!(incremental_outcome.event_count, reader_outcome.event_count);
        assert_eq!(
            borrowed_reader_outcome.event_count,
            reader_outcome.event_count
        );
        assert_eq!(
            incremental_outcome.decoded_bytes,
            reader_outcome.decoded_bytes
        );
        assert_eq!(
            borrowed_reader_outcome.decoded_bytes,
            reader_outcome.decoded_bytes
        );
        assert_eq!(
            incremental_outcome
                .terminal_error
                .as_ref()
                .map(ToString::to_string),
            reader_outcome
                .terminal_error
                .as_ref()
                .map(ToString::to_string)
        );
        assert_eq!(
            borrowed_reader_outcome
                .terminal_error
                .as_ref()
                .map(ToString::to_string),
            reader_outcome
                .terminal_error
                .as_ref()
                .map(ToString::to_string)
        );
    }

    fn event_progress_stdout(event: &BuildEvent) -> &str {
        let Some(crate::proto::build_event::Payload::Progress(progress)) = event.payload.as_ref()
        else {
            panic!("missing progress payload");
        };
        &progress.stdout
    }
}
