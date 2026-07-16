use std::marker::PhantomData;

use buffa::{Message, bytes::Buf};
use tonic::{
    Status,
    codec::{Codec, DecodeBuf, Decoder, EncodeBuf, Encoder},
};

use crate::proto::{
    Empty, PublishBuildToolEventStreamRequestOwnedView, PublishBuildToolEventStreamResponse,
    PublishBuildToolEventStreamResponseOwnedView, PublishLifecycleEventRequestOwnedView,
};

pub trait DecodeOwnedView: Sized + Send + 'static {
    fn decode(bytes: buffa::bytes::Bytes) -> Result<Self, buffa::DecodeError>;
}

impl DecodeOwnedView for PublishLifecycleEventRequestOwnedView {
    fn decode(bytes: buffa::bytes::Bytes) -> Result<Self, buffa::DecodeError> {
        Self::decode(bytes)
    }
}

impl DecodeOwnedView for PublishBuildToolEventStreamRequestOwnedView {
    fn decode(bytes: buffa::bytes::Bytes) -> Result<Self, buffa::DecodeError> {
        Self::decode(bytes)
    }
}

impl DecodeOwnedView for PublishBuildToolEventStreamResponseOwnedView {
    fn decode(bytes: buffa::bytes::Bytes) -> Result<Self, buffa::DecodeError> {
        Self::decode(bytes)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct BuffaCodec<Decode, Encode>(PhantomData<fn() -> (Decode, Encode)>);

impl<Decode, Encode> Default for BuffaCodec<Decode, Encode> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

impl<Decode, Encode> Codec for BuffaCodec<Decode, Encode>
where
    Decode: DecodeOwnedView,
    Encode: Message + Send + 'static,
{
    type Encode = Encode;
    type Decode = Decode;
    type Encoder = BuffaEncoder<Encode>;
    type Decoder = BuffaDecoder<Decode>;

    fn encoder(&mut self) -> Self::Encoder {
        BuffaEncoder(PhantomData)
    }

    fn decoder(&mut self) -> Self::Decoder {
        BuffaDecoder(PhantomData)
    }
}

pub(crate) type LifecycleCodec = BuffaCodec<PublishLifecycleEventRequestOwnedView, Empty>;
pub(crate) type BuildToolStreamCodec =
    BuffaCodec<PublishBuildToolEventStreamRequestOwnedView, PublishBuildToolEventStreamResponse>;

pub struct BuffaEncoder<M>(PhantomData<fn() -> M>);

impl<M> Encoder for BuffaEncoder<M>
where
    M: Message,
{
    type Item = M;
    type Error = Status;

    fn encode(&mut self, item: Self::Item, dst: &mut EncodeBuf<'_>) -> Result<(), Self::Error> {
        item.encode(dst);
        Ok(())
    }
}

pub struct BuffaDecoder<M>(PhantomData<fn() -> M>);

impl<M> Decoder for BuffaDecoder<M>
where
    M: DecodeOwnedView,
{
    type Item = M;
    type Error = Status;

    fn decode(&mut self, src: &mut DecodeBuf<'_>) -> Result<Option<Self::Item>, Self::Error> {
        let bytes = src.copy_to_bytes(src.remaining());
        M::decode(bytes)
            .map(Some)
            .map_err(|error| Status::invalid_argument(format!("invalid BES protobuf: {error}")))
    }
}
