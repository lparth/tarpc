// Copyright 2016 Google Inc. All Rights Reserved.
//
// Licensed under the MIT License, <LICENSE or http://opensource.org/licenses/MIT>.
// This file may not be copied, modified, or distributed except according to those terms.

use {serde, tokio_core};
use bincode::{self, Infinite};
use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use std::io::{self, Cursor};
use std::marker::PhantomData;
use std::mem;
use tokio_core::io::{EasyBuf, Framed, Io};
use tokio_proto::multiplex::{ClientProto, ServerProto};
use tokio_proto::streaming::multiplex::RequestId;

// `Encode` is the type that `Codec` encodes. `Decode` is the type it decodes.
pub struct Codec<Encode, Decode> {
    max_payload_size: u64,
    state: CodecState,
    _phantom_data: PhantomData<(Encode, Decode)>,
}

enum CodecState {
    Id,
    Len { id: u64 },
    Payload { id: u64, len: u64 },
}

impl<Encode, Decode> Codec<Encode, Decode> {
    fn new(max_payload_size: u64) -> Self {
        Codec {
            max_payload_size: max_payload_size,
            state: CodecState::Id,
            _phantom_data: PhantomData,
        }
    }
}

fn too_big(payload_size: u64, max_payload_size: u64) -> io::Error {
    warn!("Not sending too-big packet of size {} (max is {})",
          payload_size, max_payload_size);
    io::Error::new(io::ErrorKind::InvalidData,
                   format!("Maximum payload size is {} bytes but got a payload of {}",
                           max_payload_size, payload_size))
}

impl<Encode, Decode> tokio_core::io::Codec for Codec<Encode, Decode>
    where Encode: serde::Serialize,
          Decode: serde::Deserialize
{
    type Out = (RequestId, Encode);
    type In = (RequestId, Result<Decode, bincode::Error>);

    fn encode(&mut self, (id, message): Self::Out, buf: &mut Vec<u8>) -> io::Result<()> {
        buf.write_u64::<BigEndian>(id).unwrap();
        trace!("Encoded request id = {} as {:?}", id, buf);
        let payload_size = bincode::serialized_size(&message);
        if payload_size > self.max_payload_size {
            return Err(too_big(payload_size, self.max_payload_size));
        }
        buf.write_u64::<BigEndian>(payload_size).unwrap();
        bincode::serialize_into(buf,
                                &message,
                                Infinite)
            .map_err(|serialize_err| io::Error::new(io::ErrorKind::Other, serialize_err))?;
        trace!("Encoded buffer: {:?}", buf);
        Ok(())
    }

    fn decode(&mut self, buf: &mut EasyBuf) -> Result<Option<Self::In>, io::Error> {
        use self::CodecState::*;
        trace!("Codec::decode: {:?}", buf.as_slice());

        loop {
            match self.state {
                Id if buf.len() < mem::size_of::<u64>() => {
                    trace!("--> Buf len is {}; waiting for 8 to parse id.", buf.len());
                    return Ok(None);
                }
                Id => {
                    let mut id_buf = buf.drain_to(mem::size_of::<u64>());
                    let id = Cursor::new(&mut id_buf).read_u64::<BigEndian>()?;
                    trace!("--> Parsed id = {} from {:?}", id, id_buf.as_slice());
                    self.state = Len { id: id };
                }
                Len { .. } if buf.len() < mem::size_of::<u64>() => {
                    trace!("--> Buf len is {}; waiting for 8 to parse packet length.",
                           buf.len());
                    return Ok(None);
                }
                Len { id } => {
                    let len_buf = buf.drain_to(mem::size_of::<u64>());
                    let len = Cursor::new(len_buf).read_u64::<BigEndian>()?;
                    trace!("--> Parsed payload length = {}, remaining buffer length = {}",
                           len,
                           buf.len());
                    if len > self.max_payload_size {
                        return Err(too_big(len, self.max_payload_size));
                    }
                    self.state = Payload { id: id, len: len };
                }
                Payload { len, .. } if buf.len() < len as usize => {
                    trace!("--> Buf len is {}; waiting for {} to parse payload.",
                           buf.len(),
                           len);
                    return Ok(None);
                }
                Payload { id, len } => {
                    let payload = buf.drain_to(len as usize);
                    let result = bincode::deserialize_from(&mut Cursor::new(payload),
                                                           Infinite);
                    // Reset the state machine because, either way, we're done processing this
                    // message.
                    self.state = Id;

                    return Ok(Some((id, result)));
                }
            }
        }
    }
}

/// Implements the `multiplex::ServerProto` trait.
pub struct Proto<Encode, Decode> {
    max_payload_size: u64,
    _phantom_data: PhantomData<(Encode, Decode)>,
}

impl<Encode, Decode> Proto<Encode, Decode> {
    /// Returns a new `Proto`.
    pub fn new(max_payload_size: u64) -> Self {
        Proto {
            max_payload_size: max_payload_size,
            _phantom_data: PhantomData
        }
    }
}

impl<T, Encode, Decode> ServerProto<T> for Proto<Encode, Decode>
    where T: Io + 'static,
          Encode: serde::Serialize + 'static,
          Decode: serde::Deserialize + 'static
{
    type Response = Encode;
    type Request = Result<Decode, bincode::Error>;
    type Transport = Framed<T, Codec<Encode, Decode>>;
    type BindTransport = Result<Self::Transport, io::Error>;

    fn bind_transport(&self, io: T) -> Self::BindTransport {
        Ok(io.framed(Codec::new(self.max_payload_size)))
    }
}

impl<T, Encode, Decode> ClientProto<T> for Proto<Encode, Decode>
    where T: Io + 'static,
          Encode: serde::Serialize + 'static,
          Decode: serde::Deserialize + 'static
{
    type Response = Result<Decode, bincode::Error>;
    type Request = Encode;
    type Transport = Framed<T, Codec<Encode, Decode>>;
    type BindTransport = Result<Self::Transport, io::Error>;

    fn bind_transport(&self, io: T) -> Self::BindTransport {
        Ok(io.framed(Codec::new(self.max_payload_size)))
    }
}

#[test]
fn serialize() {
    use tokio_core::io::Codec as TokioCodec;

    const MSG: (u64, (char, char, char)) = (4, ('a', 'b', 'c'));
    let mut buf = EasyBuf::new();
    let mut vec = Vec::new();

    // Serialize twice to check for idempotence.
    for _ in 0..2 {
        let mut codec: Codec<(char, char, char), (char, char, char)> = Codec::new(2_000_000);
        codec.encode(MSG, &mut vec).unwrap();
        buf.get_mut().append(&mut vec);
        let actual: Result<Option<(u64, Result<(char, char, char), bincode::Error>)>, io::Error> =
            codec.decode(&mut buf);

        match actual {
            Ok(Some((id, ref v))) if id == MSG.0 && *v.as_ref().unwrap() == MSG.1 => {}
            bad => panic!("Expected {:?}, but got {:?}", Some(MSG), bad),
        }

        assert!(buf.get_mut().is_empty(),
                "Expected empty buf but got {:?}",
                *buf.get_mut());
    }
}

#[test]
fn deserialize_big() {
    use tokio_core::io::Codec as TokioCodec;
    let mut codec: Codec<Vec<u8>, Vec<u8>> = Codec::new(24);

    let mut vec = Vec::new();
    assert_eq!(codec.encode((0, vec![0; 24]), &mut vec).err().unwrap().kind(),
               io::ErrorKind::InvalidData);

    let mut buf = EasyBuf::new();
    // Header
    buf.get_mut().append(&mut vec![0; 8]);
    // Len
    buf.get_mut().append(&mut vec![0, 0, 0, 0, 0, 0, 0, 25]);
    assert_eq!(codec.decode(&mut buf).err().unwrap().kind(),
               io::ErrorKind::InvalidData);
}
