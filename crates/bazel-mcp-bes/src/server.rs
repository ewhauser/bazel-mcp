use std::{
    collections::HashMap,
    convert::Infallible,
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use bazel_mcp_bep::{DEFAULT_MAX_FRAME_BYTES, DEFAULT_MAX_STREAM_BYTES, DEFAULT_MAX_STREAM_EVENTS};
use buffa::MessageField;
use thiserror::Error;
use tokio::{fs::OpenOptions, io::AsyncWriteExt, sync::watch};
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tokio_util::sync::CancellationToken;
use tonic::{
    Request, Response, Status, Streaming,
    body::Body as TonicBody,
    codegen::{Body, BoxFuture, Service, StdError, http},
    server::{Grpc, NamedService, StreamingService, UnaryService},
    transport::Server,
};

use crate::{
    codec::{BuildToolStreamCodec, LifecycleCodec},
    proto::{
        Empty, PublishBuildToolEventStreamRequestOwnedView, PublishBuildToolEventStreamResponse,
        PublishLifecycleEventRequestOwnedView, StreamId, build_event::EventView,
    },
};

const SERVICE_NAME: &str = "google.devtools.build.v1.PublishBuildEvent";
const LIFECYCLE_PATH: &str = "/google.devtools.build.v1.PublishBuildEvent/PublishLifecycleEvent";
const BUILD_TOOL_STREAM_PATH: &str =
    "/google.devtools.build.v1.PublishBuildEvent/PublishBuildToolEventStream";
const BAZEL_EVENT_TYPE_SUFFIX: &str = "/build_event_stream.BuildEvent";
const MAX_GRPC_REQUEST_BYTES: usize = DEFAULT_MAX_FRAME_BYTES + 64 * 1024;
const MAX_STREAM_ID_FIELD_BYTES: usize = 128;
const MAX_TYPE_URL_BYTES: usize = 256;

type CaptureResult = Result<CaptureStats, String>;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CaptureStats {
    pub event_count: usize,
    pub bep_bytes: usize,
}

#[derive(Debug, Error)]
pub enum BesError {
    #[error("could not bind the loopback BES server: {0}")]
    Bind(#[source] io::Error),
    #[error("could not register BES capture for invocation {0}")]
    DuplicateInvocation(String),
    #[error("BES capture registry lock was poisoned")]
    RegistryPoisoned,
    #[error("BES capture failed: {0}")]
    Capture(String),
    #[error("timed out waiting for BES capture completion")]
    CaptureTimeout,
    #[error("BES capture ended without a completion result")]
    CaptureClosed,
}

struct CaptureState {
    path: PathBuf,
    active: AtomicBool,
    completion: watch::Sender<Option<CaptureResult>>,
}

type Captures = Arc<Mutex<HashMap<String, Arc<CaptureState>>>>;

struct ServerInner {
    endpoint: String,
    captures: Captures,
    shutdown: CancellationToken,
}

impl Drop for ServerInner {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

#[derive(Clone)]
pub struct BesServer {
    inner: Arc<ServerInner>,
}

impl BesServer {
    pub async fn start() -> Result<Self, BesError> {
        let listener =
            tokio::net::TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
                .await
                .map_err(BesError::Bind)?;
        let address = listener.local_addr().map_err(BesError::Bind)?;
        let endpoint = format!("grpc://{address}");
        let captures = Arc::new(Mutex::new(HashMap::new()));
        let shutdown = CancellationToken::new();
        let service = PublishBuildEventService {
            captures: captures.clone(),
        };
        let server_shutdown = shutdown.clone();
        tokio::spawn(async move {
            let result = Server::builder()
                .add_service(service)
                .serve_with_incoming_shutdown(
                    TcpListenerStream::new(listener),
                    server_shutdown.cancelled_owned(),
                )
                .await;
            if let Err(error) = result {
                tracing::error!(%error, "loopback BES server stopped unexpectedly");
            }
        });
        tracing::info!(%endpoint, "started loopback BES server");
        Ok(Self {
            inner: Arc::new(ServerInner {
                endpoint,
                captures,
                shutdown,
            }),
        })
    }

    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.inner.endpoint
    }

    pub fn register(
        &self,
        invocation_id: impl Into<String>,
        path: impl Into<PathBuf>,
    ) -> Result<BesCapture, BesError> {
        let invocation_id = invocation_id.into();
        let (completion, receiver) = watch::channel(None);
        let state = Arc::new(CaptureState {
            path: path.into(),
            active: AtomicBool::new(false),
            completion,
        });
        let mut captures = self
            .inner
            .captures
            .lock()
            .map_err(|_| BesError::RegistryPoisoned)?;
        match captures.entry(invocation_id.clone()) {
            std::collections::hash_map::Entry::Occupied(_) => {
                return Err(BesError::DuplicateInvocation(invocation_id));
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(state.clone());
            }
        }
        Ok(BesCapture {
            invocation_id,
            captures: self.inner.captures.clone(),
            state,
            completion: receiver,
        })
    }
}

pub struct BesCapture {
    invocation_id: String,
    captures: Captures,
    state: Arc<CaptureState>,
    completion: watch::Receiver<Option<CaptureResult>>,
}

impl BesCapture {
    pub async fn finish(mut self, timeout: Duration) -> Result<CaptureStats, BesError> {
        let result = tokio::time::timeout(timeout, async {
            loop {
                if let Some(result) = self.completion.borrow().clone() {
                    break result.map_err(BesError::Capture);
                }
                self.completion
                    .changed()
                    .await
                    .map_err(|_| BesError::CaptureClosed)?;
            }
        })
        .await
        .map_err(|_| BesError::CaptureTimeout)?;
        self.remove();
        result
    }

    fn remove(&self) {
        if let Ok(mut captures) = self.captures.lock() {
            let is_same_capture = captures
                .get(&self.invocation_id)
                .is_some_and(|registered| Arc::ptr_eq(registered, &self.state));
            if is_same_capture {
                captures.remove(&self.invocation_id);
            }
        }
    }
}

impl Drop for BesCapture {
    fn drop(&mut self) {
        self.remove();
    }
}

#[derive(Clone)]
struct PublishBuildEventService {
    captures: Captures,
}

impl<B> Service<http::Request<B>> for PublishBuildEventService
where
    B: Body + Send + 'static,
    B::Error: Into<StdError> + Send + 'static,
{
    type Response = http::Response<TonicBody>;
    type Error = Infallible;
    type Future = BoxFuture<Self::Response, Self::Error>;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: http::Request<B>) -> Self::Future {
        match request.uri().path() {
            LIFECYCLE_PATH => {
                struct LifecycleService;

                impl UnaryService<PublishLifecycleEventRequestOwnedView> for LifecycleService {
                    type Response = Empty;
                    type Future = BoxFuture<Response<Self::Response>, Status>;

                    fn call(
                        &mut self,
                        _request: Request<PublishLifecycleEventRequestOwnedView>,
                    ) -> Self::Future {
                        Box::pin(async { Ok(Response::new(Empty::default())) })
                    }
                }

                Box::pin(async move {
                    let mut grpc = Grpc::new(LifecycleCodec::default())
                        .max_decoding_message_size(MAX_GRPC_REQUEST_BYTES);
                    Ok(grpc.unary(LifecycleService, request).await)
                })
            }
            BUILD_TOOL_STREAM_PATH => {
                struct BuildToolStreamService {
                    captures: Captures,
                }

                impl StreamingService<PublishBuildToolEventStreamRequestOwnedView> for BuildToolStreamService {
                    type Response = PublishBuildToolEventStreamResponse;
                    type ResponseStream = ReceiverStream<Result<Self::Response, Status>>;
                    type Future = BoxFuture<Response<Self::ResponseStream>, Status>;

                    fn call(
                        &mut self,
                        request: Request<Streaming<PublishBuildToolEventStreamRequestOwnedView>>,
                    ) -> Self::Future {
                        let captures = self.captures.clone();
                        Box::pin(async move {
                            let (responses, receiver) = tokio::sync::mpsc::channel(32);
                            tokio::spawn(ingest_stream(captures, request.into_inner(), responses));
                            Ok(Response::new(ReceiverStream::new(receiver)))
                        })
                    }
                }

                let service = BuildToolStreamService {
                    captures: self.captures.clone(),
                };
                Box::pin(async move {
                    let mut grpc = Grpc::new(BuildToolStreamCodec::default())
                        .max_decoding_message_size(MAX_GRPC_REQUEST_BYTES);
                    Ok(grpc.streaming(service, request).await)
                })
            }
            _ => Box::pin(async move {
                Ok(http::Response::builder()
                    .status(200)
                    .header("grpc-status", "12")
                    .header("content-type", "application/grpc")
                    .body(TonicBody::empty())
                    .expect("static gRPC fallback response is valid"))
            }),
        }
    }
}

impl NamedService for PublishBuildEventService {
    const NAME: &'static str = SERVICE_NAME;
}

async fn ingest_stream(
    captures: Captures,
    mut input: Streaming<PublishBuildToolEventStreamRequestOwnedView>,
    responses: tokio::sync::mpsc::Sender<Result<PublishBuildToolEventStreamResponse, Status>>,
) {
    let Some(first) = recv_request(&mut input, &responses).await else {
        return;
    };
    let state_result = {
        match request_invocation_id(&first) {
            Ok(invocation_id) => match captures.lock() {
                Ok(captures) => Ok(captures.get(invocation_id).cloned()),
                Err(_) => Err(Status::internal("BES capture registry lock was poisoned")),
            },
            Err(error) => Err(error),
        }
    };
    let state = match state_result {
        Ok(state) => state,
        Err(error) => {
            send_status(&responses, error).await;
            return;
        }
    };
    let Some(state) = state else {
        send_status(
            &responses,
            Status::not_found("BES stream does not match an active Bazel MCP invocation"),
        )
        .await;
        return;
    };
    if state.active.swap(true, Ordering::AcqRel) {
        let message = "a BES stream is already active for this invocation";
        complete(&state, Err(message.to_owned()));
        send_status(&responses, Status::already_exists(message)).await;
        return;
    }
    let result = capture_stream(&state.path, first, &mut input, &responses).await;
    if let Err(error) = &result {
        let _ = responses.send(Err(Status::internal(error.clone()))).await;
    }
    complete(&state, result);
}

async fn capture_stream(
    path: &Path,
    first: PublishBuildToolEventStreamRequestOwnedView,
    input: &mut Streaming<PublishBuildToolEventStreamRequestOwnedView>,
    responses: &tokio::sync::mpsc::Sender<Result<PublishBuildToolEventStreamResponse, Status>>,
) -> CaptureResult {
    let file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)
        .await
        .map_err(|error| format!("open {}: {error}", path.display()))?;
    #[cfg(unix)]
    file.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))
        .await
        .map_err(|error| format!("set permissions on {}: {error}", path.display()))?;
    let mut writer = file;
    let first_ordered = ordered_event(&first).map_err(|status| status.message().to_owned())?;
    let first_stream_id = first_ordered
        .stream_id
        .as_option()
        .ok_or_else(|| "BES request omitted stream_id".to_owned())?;
    let identity = StreamIdentity::from_view(first_stream_id)?;
    let mut expected_sequence = 1_i64;
    let mut request_count = 0_usize;
    let mut stats = CaptureStats::default();
    let mut framed_event = Vec::new();
    let mut saw_finished = false;
    let mut current = Some(first);

    loop {
        let request = if let Some(request) = current.take() {
            request
        } else {
            match input.message().await {
                Ok(Some(request)) => request,
                Ok(None) => break,
                Err(status) => return Err(format!("receive BES request: {status}")),
            }
        };
        let ordered = ordered_event(&request).map_err(|status| status.message().to_owned())?;
        request_count = request_count.saturating_add(1);
        if request_count > DEFAULT_MAX_STREAM_EVENTS.saturating_add(1) {
            return Err(format!(
                "BES request count exceeds limit {}",
                DEFAULT_MAX_STREAM_EVENTS + 1
            ));
        }
        let stream_id = ordered
            .stream_id
            .as_option()
            .ok_or_else(|| "BES request omitted stream_id".to_owned())?;
        identity.validate(stream_id)?;
        if ordered.sequence_number != expected_sequence {
            return Err(format!(
                "BES sequence number {} did not match expected {}",
                ordered.sequence_number, expected_sequence
            ));
        }
        let event = ordered
            .event
            .as_option()
            .ok_or_else(|| "BES request omitted build event".to_owned())?;
        if saw_finished {
            return Err("BES request followed BuildComponentStreamFinished".to_owned());
        }
        match event.event.as_ref() {
            Some(EventView::BazelEvent(any)) => {
                if any.type_url.len() > MAX_TYPE_URL_BYTES
                    || (!any.type_url.is_empty()
                        && !any.type_url.ends_with(BAZEL_EVENT_TYPE_SUFFIX))
                {
                    return Err("unexpected BES Any type URL".to_owned());
                }
                write_bep_frame(&mut writer, any.value, &mut framed_event, &mut stats).await?;
            }
            Some(EventView::ComponentStreamFinished(finished)) => {
                if finished.r#type != 1 {
                    return Err(format!(
                        "BES component stream finished with unsupported type {}",
                        finished.r#type
                    ));
                }
                saw_finished = true;
            }
            None => {}
        }
        let response = PublishBuildToolEventStreamResponse {
            stream_id: MessageField::some(identity.to_owned()),
            sequence_number: ordered.sequence_number,
        };
        responses
            .send(Ok(response))
            .await
            .map_err(|_| "BES client stopped accepting acknowledgements".to_owned())?;
        expected_sequence = expected_sequence
            .checked_add(1)
            .ok_or_else(|| "BES sequence number overflow".to_owned())?;
    }
    writer
        .flush()
        .await
        .map_err(|error| format!("flush {}: {error}", path.display()))?;
    if !saw_finished {
        return Err("BES stream ended without BuildComponentStreamFinished".to_owned());
    }
    Ok(stats)
}

fn ordered_event(
    request: &PublishBuildToolEventStreamRequestOwnedView,
) -> Result<&crate::proto::OrderedBuildEventView<'_>, Status> {
    request
        .ordered_build_event()
        .as_option()
        .ok_or_else(|| Status::invalid_argument("BES request omitted ordered_build_event"))
}

fn request_invocation_id(
    request: &PublishBuildToolEventStreamRequestOwnedView,
) -> Result<&str, Status> {
    let ordered = ordered_event(request)?;
    let stream_id = ordered
        .stream_id
        .as_option()
        .ok_or_else(|| Status::invalid_argument("BES request omitted stream_id"))?;
    if stream_id.invocation_id.is_empty()
        || stream_id.invocation_id.len() > MAX_STREAM_ID_FIELD_BYTES
    {
        Err(Status::invalid_argument(
            "BES stream_id omitted invocation_id",
        ))
    } else {
        Ok(stream_id.invocation_id)
    }
}

async fn recv_request(
    input: &mut Streaming<PublishBuildToolEventStreamRequestOwnedView>,
    responses: &tokio::sync::mpsc::Sender<Result<PublishBuildToolEventStreamResponse, Status>>,
) -> Option<PublishBuildToolEventStreamRequestOwnedView> {
    match input.message().await {
        Ok(Some(request)) => Some(request),
        Ok(None) => {
            send_status(
                responses,
                Status::invalid_argument("BES build-tool stream was empty"),
            )
            .await;
            None
        }
        Err(status) => {
            send_status(responses, status).await;
            None
        }
    }
}

async fn send_status<T>(responses: &tokio::sync::mpsc::Sender<Result<T, Status>>, status: Status) {
    let _ = responses.send(Err(status)).await;
}

fn complete(state: &CaptureState, result: CaptureResult) {
    state.completion.send_replace(Some(result));
}

async fn write_bep_frame(
    writer: &mut tokio::fs::File,
    frame: &[u8],
    framed_event: &mut Vec<u8>,
    stats: &mut CaptureStats,
) -> Result<(), String> {
    if frame.len() > DEFAULT_MAX_FRAME_BYTES {
        return Err(format!(
            "BES frame length {} exceeds limit {}",
            frame.len(),
            DEFAULT_MAX_FRAME_BYTES
        ));
    }
    let next_bytes = stats.bep_bytes.saturating_add(frame.len());
    if next_bytes > DEFAULT_MAX_STREAM_BYTES {
        return Err(format!(
            "BES stream bytes {next_bytes} exceed limit {DEFAULT_MAX_STREAM_BYTES}"
        ));
    }
    if stats.event_count >= DEFAULT_MAX_STREAM_EVENTS {
        return Err(format!(
            "BES event count exceeds limit {DEFAULT_MAX_STREAM_EVENTS}"
        ));
    }
    let mut prefix = [0_u8; 10];
    let prefix_len = encode_varint(frame.len() as u64, &mut prefix);
    framed_event.clear();
    framed_event.reserve(prefix_len + frame.len());
    framed_event.extend_from_slice(&prefix[..prefix_len]);
    framed_event.extend_from_slice(frame);
    writer
        .write_all(framed_event)
        .await
        .map_err(|error| format!("write BEP frame: {error}"))?;
    stats.event_count += 1;
    stats.bep_bytes = next_bytes;
    Ok(())
}

fn encode_varint(mut value: u64, output: &mut [u8; 10]) -> usize {
    let mut length = 0;
    while value >= 0x80 {
        output[length] = (value as u8 & 0x7f) | 0x80;
        value >>= 7;
        length += 1;
    }
    output[length] = value as u8;
    length + 1
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct StreamIdentity {
    build_id: String,
    invocation_id: String,
    component: i32,
}

impl StreamIdentity {
    fn from_view(stream_id: &crate::proto::StreamIdView<'_>) -> Result<Self, String> {
        if stream_id.build_id.len() > MAX_STREAM_ID_FIELD_BYTES
            || stream_id.invocation_id.len() > MAX_STREAM_ID_FIELD_BYTES
        {
            return Err("BES stream_id field exceeds length limit".to_owned());
        }
        Ok(Self {
            build_id: stream_id.build_id.to_owned(),
            invocation_id: stream_id.invocation_id.to_owned(),
            component: stream_id.component,
        })
    }

    fn validate(&self, stream_id: &crate::proto::StreamIdView<'_>) -> Result<(), String> {
        if self.build_id == stream_id.build_id
            && self.invocation_id == stream_id.invocation_id
            && self.component == stream_id.component
        {
            Ok(())
        } else {
            Err("BES stream_id changed within one RPC".to_owned())
        }
    }

    fn to_owned(&self) -> StreamId {
        StreamId {
            build_id: self.build_id.clone(),
            invocation_id: self.invocation_id.clone(),
            component: self.component,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use buffa::Message;
    use tempfile::tempdir;
    use tonic::{
        client::Grpc as ClientGrpc, codegen::http::uri::PathAndQuery, transport::Endpoint,
    };

    use crate::{
        codec::BuffaCodec,
        proto::{
            Any, BuildComponentStreamFinished, BuildEvent, OrderedBuildEvent,
            PublishBuildToolEventStreamRequest, PublishBuildToolEventStreamResponseOwnedView,
            build_event::Event,
        },
    };

    #[test]
    fn varint_prefixes_match_bep_framing() {
        for value in [0, 1, 127, 128, 16_384, u32::MAX as u64] {
            let mut bytes = [0_u8; 10];
            let length = encode_varint(value, &mut bytes);
            let mut decoded = 0_u64;
            for (index, byte) in bytes[..length].iter().enumerate() {
                decoded |= u64::from(byte & 0x7f) << (index * 7);
            }
            assert_eq!(decoded, value);
        }
    }

    #[tokio::test]
    async fn streams_bazel_events_over_grpc_and_retains_bep_framing() {
        let root = tempdir().unwrap();
        let path = root.path().join("events.bep");
        let server = BesServer::start().await.unwrap();
        let invocation_id = "019f6b1e-dbf1-7090-9290-747e9021d447";
        let capture = server.register(invocation_id, &path).unwrap();
        let bep_frame = bazel_mcp_bep::proto::BuildEvent::default().encode_to_vec();
        let stream_id = StreamId {
            build_id: "build-id".to_owned(),
            invocation_id: invocation_id.to_owned(),
            component: 3,
        };
        let requests = vec![
            stream_request(
                stream_id.clone(),
                1,
                Event::BazelEvent(Box::new(Any {
                    type_url: "type.googleapis.com/build_event_stream.BuildEvent".to_owned(),
                    value: bep_frame.clone(),
                })),
            ),
            stream_request(
                stream_id,
                2,
                Event::ComponentStreamFinished(Box::new(BuildComponentStreamFinished {
                    r#type: 1,
                })),
            ),
        ];
        let uri = server.endpoint().replacen("grpc://", "http://", 1);
        let channel = Endpoint::from_shared(uri).unwrap().connect().await.unwrap();
        let mut client = ClientGrpc::new(channel);
        client.ready().await.unwrap();
        let response = client
            .streaming(
                Request::new(tokio_stream::iter(requests)),
                PathAndQuery::from_static(BUILD_TOOL_STREAM_PATH),
                BuffaCodec::<
                    PublishBuildToolEventStreamResponseOwnedView,
                    PublishBuildToolEventStreamRequest,
                >::default(),
            )
            .await
            .unwrap();
        let mut acknowledgements = response.into_inner();
        assert_eq!(
            acknowledgements
                .message()
                .await
                .unwrap()
                .unwrap()
                .sequence_number(),
            1
        );
        assert_eq!(
            acknowledgements
                .message()
                .await
                .unwrap()
                .unwrap()
                .sequence_number(),
            2
        );
        assert!(acknowledgements.message().await.unwrap().is_none());
        let stats = capture.finish(Duration::from_secs(1)).await.unwrap();
        assert_eq!(stats.event_count, 1);
        assert_eq!(stats.bep_bytes, bep_frame.len());

        let mut file = std::fs::File::open(path).unwrap();
        assert_eq!(
            bazel_mcp_bep::read_frame(&mut file, DEFAULT_MAX_FRAME_BYTES)
                .unwrap()
                .unwrap(),
            bep_frame
        );
        assert!(
            bazel_mcp_bep::read_frame(&mut file, DEFAULT_MAX_FRAME_BYTES)
                .unwrap()
                .is_none()
        );
    }

    fn stream_request(
        stream_id: StreamId,
        sequence_number: i64,
        event: Event,
    ) -> PublishBuildToolEventStreamRequest {
        PublishBuildToolEventStreamRequest {
            ordered_build_event: MessageField::some(OrderedBuildEvent {
                stream_id: MessageField::some(stream_id),
                sequence_number,
                event: MessageField::some(BuildEvent { event: Some(event) }),
            }),
        }
    }
}
