#![doc(html_root_url = "http://docs.rs/tokio-qapi/0.5.0")]
#![feature(futures_api, async_await, await_macro, arbitrary_self_types)]

use qapi_spec as spec;

#[cfg(feature = "qapi-qmp")]
pub use qapi_qmp as qmp;

#[cfg(feature = "qapi-qga")]
pub use qapi_qga as qga;

pub use qapi_spec::{Any, Dictionary, Empty, Command, Event, Error, ErrorClass, Timestamp};

use std::collections::BTreeMap;
use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
use std::mem::replace;
use std::{io, str, usize};
use result::OptionResultExt;
use futures::compat::{Stream01CompatExt, Future01CompatExt, Compat01As03, Compat};
use futures::channel::oneshot;
use futures::io::{AsyncRead, AsyncWrite, AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio_codec::{Framed, FramedRead, LinesCodec, Encoder, Decoder};
use futures::{Future, TryFutureExt, Poll, Sink, Stream, StreamExt, future, try_ready, try_join};
use futures::stream::{FusedStream, unfold};
use std::pin::Pin;
use std::marker::Unpin;
//use futures::io::{AsyncRead as _AsyncRead, AsyncWrite as _AsyncWrite, AsyncReadExt, ReadHalf, WriteHalf};
use futures::lock::Mutex;
//use futures::sync::BiLock;
//use futures::task::{self, Task};
use bytes::BytesMut;
use bytes::buf::FromBuf;
use log::{trace, debug};

type QapiStreamLines<S> = Compat01As03<FramedRead<Compat<S>, LinesCodec>>;

type QapiCommandMap = BTreeMap<u64, oneshot::Sender<Result<Any, qapi_spec::Error>>>;

pub struct QapiStream<W> {
    pending: QapiShared,
    write_lock: Mutex<W>,
    supports_oob: bool,
    id_counter: AtomicUsize,
}

impl<W> QapiStream<W> {
    fn new(write: W, pending: QapiShared, supports_oob: bool) -> Self {
        QapiStream {
            pending,
            write_lock: Mutex::new(write),
            supports_oob,
            id_counter: AtomicUsize::new(0),
        }
    }

    fn next_oob_id(&self) -> u64 {
        self.id_counter.fetch_add(1, Ordering::Relaxed) as _
    }
}

type QapiShared = Arc<Mutex<QapiCommandMap>>;

#[cfg(any(feature = "qapi-qmp", feature = "qapi-qga"))]
pub struct QapiEvents<R> {
    lines: QapiStreamLines<R>,
    pending: QapiShared,
    supports_oob: bool,
}

#[cfg(any(feature = "qapi-qmp", feature = "qapi-qga"))]
impl<R> QapiEvents<R> {
    fn new(lines: QapiStreamLines<R>, supports_oob: bool) -> (Self, QapiShared) {
        let pending: QapiShared = Arc::new(Mutex::new(Default::default()));

        (QapiEvents {
            lines,
            pending: pending.clone(),
            supports_oob,
        }, pending)
    }
}

#[cfg(any(feature = "qapi-qmp", feature = "qapi-qga"))]
enum QapiEventsMessage {
    Response {
        id: u64,
    },
    #[cfg(feature = "qapi-qmp")]
    Event(qapi_qmp::Event),
    Eof,
}

#[cfg(any(feature = "qapi-qmp", feature = "qapi-qga"))]
impl<R: AsyncRead> QapiEvents<R> {
    async fn process_response(self_supports_oob: bool, self_pending: &QapiShared, res: qapi_spec::Response<Any>) -> io::Result<u64> {
        let id = match (res.id().and_then(|id| id.as_u64()), self_supports_oob) {
            (Some(id), true) => id,
            (None, false) => Default::default(),
            (None, true) => return Err(io::Error::new(io::ErrorKind::InvalidData, format!("QAPI expected response with numeric ID, got {:?}", res.id()))),
            (Some(..), false) => return Err(io::Error::new(io::ErrorKind::InvalidData, format!("QAPI expected response without ID, got {:?}", res.id()))),
        };
        let mut pending = await!(self_pending.lock());
        if let Some(sender) = pending.remove(&id) {
            drop(pending);
            sender.send(res.result())
                .map_err(|e| unimplemented!())
                .map(|()| id)
        } else {
            Err(io::Error::new(io::ErrorKind::InvalidData, format!("unknown QAPI response with ID {:?}", res.id())))
        }
    }

    async fn process_message(&mut self) -> io::Result<QapiEventsMessage> {
        let msg = match await!(self.lines.next()).invert()? {
            #[cfg(feature = "qapi-qmp")]
            Some(line) => serde_json::from_str::<qapi_qmp::QmpMessage<Any>>(&line)?,
            #[cfg(not(feature = "qapi-qmp"))]
            Some(line) => serde_json::from_str::<qapi_spec::Response<Any>>(&line)?,
            None => return Ok(QapiEventsMessage::Eof),
        };
        match msg {
            #[cfg(feature = "qapi-qmp")]
            qapi_qmp::QmpMessage::Event(event) => Ok(QapiEventsMessage::Event(event)),
            //calling self here makes this async fn !Send because Compat is !Sync and it will capture &Self
            #[cfg(feature = "qapi-qmp")]
            qapi_qmp::QmpMessage::Response(res) => {
                let id = await!(Self::process_response(self.supports_oob, &self.pending, res))?;
                Ok(QapiEventsMessage::Response { id })
            },
            #[cfg(not(feature = "qapi-qmp"))]
            res => {
                let id = await!(Self::process_response(self.supports_oob, &self.pending, res))?;
                Ok(QapiEventsMessage::Response { id })
            },
        }
    }

    #[cfg(feature = "qapi-qmp")]
    pub async fn next_event(&mut self) -> io::Result<Option<qapi_qmp::Event>> {
        loop {
            match await!(self.process_message())? {
                QapiEventsMessage::Response { .. } => (),
                QapiEventsMessage::Event(event) => break Ok(Some(event)),
                QapiEventsMessage::Eof => break Ok(None),
            }
        }
    }

    #[cfg(feature = "qapi-qmp")]
    async fn next_event_(&mut self) -> io::Result<Option<qapi_qmp::Event>> {
        await!(self.next_event())
    }

    #[cfg(not(feature = "qapi-qmp"))]
    async fn next_event_(&mut self) -> io::Result<Option<()>> {
        loop {
            match await!(self.process_message())? {
                QapiEventsMessage::Response { .. } => break Ok(Some(())),
                QapiEventsMessage::Eof => break Ok(None),
            }
        }
    }

    #[cfg(feature = "qapi-qmp")]
    pub fn into_stream(self) -> impl Stream<Item=io::Result<qapi_qmp::Event>> + FusedStream {
        unfold(self, async move |mut s| {
            await!(s.next_event()).invert().map(|r| (r, s))
        })
    }

    pub async fn spin(mut self) {
        while let Some(res) = await!(self.next_event_()).invert() {
            match res {
                Ok(event) => trace!("QapiEvents::spin ignoring event: {:#?}", event),
                Err(err) => trace!("QapiEvents::spin ignoring error: {:#?}", err),
            }
        }
    }
}

#[cfg(feature = "qapi-qmp")]
impl<S: tokio_io::AsyncRead + tokio_io::AsyncWrite> QapiStream<WriteHalf<Compat01As03<S>>> {
    pub async fn open_tokio(stream: S) -> io::Result<(qmp::QapiCapabilities, Self, QapiEvents<ReadHalf<Compat01As03<S>>>)> {
        await!(Self::open(Compat01As03::new(stream)))
    }
}

#[cfg(feature = "qapi-qmp")]
impl<S: AsyncRead + AsyncWrite> QapiStream<WriteHalf<S>> {
    pub async fn open(stream: S) -> io::Result<(qmp::QapiCapabilities, Self, QapiEvents<ReadHalf<S>>)> {
        let (r, w) = stream.split();
        await!(QapiStream::open_split(r, w))
    }
}

#[cfg(feature = "qapi-qga")]
impl<S: tokio_io::AsyncRead + tokio_io::AsyncWrite> QapiStream<WriteHalf<Compat01As03<S>>> {
    pub async fn open_tokio_qga(stream: S) -> io::Result<(Self, impl Future<Output=()>)> {
        await!(Self::open_qga(Compat01As03::new(stream)))
    }
}

#[cfg(feature = "qapi-qga")]
impl<S: AsyncRead + AsyncWrite> QapiStream<WriteHalf<S>> {
    pub async fn open_qga(stream: S) -> io::Result<(Self, impl Future<Output=()>)> {
        let (r, w) = stream.split();
        await!(QapiStream::open_split_qga(r, w))
    }
}

#[cfg(feature = "qapi-qmp")]
impl<W: AsyncWrite + Unpin> QapiStream<W> {
    pub async fn open_split<R: AsyncRead>(read: R, write: W) -> io::Result<(qmp::QapiCapabilities, Self, QapiEvents<R>)> {
        let mut lines = FramedRead::new(Compat::new(read), LinesCodec::new()).compat();

        let greeting = await!(lines.next()).ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "blah"))??;
        let greeting = serde_json::from_str::<qmp::QapiCapabilities>(&greeting)?;
        let caps = greeting.capabilities();

        let supports_oob = caps.iter().any(|&c| c == qmp::QMPCapability::oob);
        let (mut events, pending) = QapiEvents::new(lines, supports_oob);
        let stream = QapiStream::new(write, pending, supports_oob);

        let mut caps = Vec::new();
        if supports_oob {
            caps.push(qmp::QMPCapability::oob);
        }

        await!(stream.negotiate_caps(&mut events, caps))?;

        Ok((greeting, stream, events))
    }

    async fn negotiate_caps<'a, R: AsyncRead>(&'a self, events: &'a mut QapiEvents<R>, caps: Vec<qmp::QMPCapability>) -> io::Result<()> {
        let caps = self.execute(qmp::qmp_capabilities {
            enable: Some(caps),
        }).and_then(|res| future::ready(res.map_err(From::from))).map_err(|err| unimplemented!("negotiation error {:?}", err));
        let events = events.process_message().and_then(|msg| future::ready(match msg {
            QapiEventsMessage::Response { id } => Ok(()),
            QapiEventsMessage::Eof =>
                Err(io::Error::new(io::ErrorKind::UnexpectedEof, "unexpected EOF when negotiating QAPI capabilities")),
            QapiEventsMessage::Event(event) =>
                Err(unimplemented!("unexpected event {:?}", event)),
        }));
        try_join!(caps, events).map(|(spec::Empty { }, ())| ())
    }
}

#[cfg(feature = "qapi-qga")]
impl<W: AsyncWrite + Unpin> QapiStream<W> {
    pub async fn open_split_qga<R: AsyncRead>(read: R, write: W) -> io::Result<(Self, impl Future<Output=()>)> {
        let mut lines = FramedRead::new(Compat::new(read), LinesCodec::new()).compat();

        let supports_oob = false;
        let (mut events, pending) = QapiEvents::new(lines, supports_oob);
        let stream = QapiStream::new(write, pending, supports_oob);

        let sync_value = &stream as *const _ as usize as _; // great randomness here um
        await!(stream.guest_sync(&mut events, sync_value))?;

        // TODO: spin will hold on to the shared reference forever ._.
        Ok((stream, events.spin()))
    }

    async fn guest_sync<'a, R: AsyncRead>(&'a self, events: &'a mut QapiEvents<R>, sync_value: u32) -> io::Result<()> {
        let sync_value = sync_value as isize;
        let sync = self.execute(qga::guest_sync {
            id: sync_value,
        }).map_err(|err| unimplemented!("negotiation error {:?}", err))
        .and_then(|res| future::ready(res.map_err(From::from)))
        .and_then(|res| future::ready(if res == sync_value {
            Ok(())
        } else {
            Err(io::Error::new(io::ErrorKind::InvalidData, "QMP sync failed"))
        }));

        let events = events.process_message().and_then(|msg| future::ready(match msg {
            QapiEventsMessage::Response { id } => Ok(()),
            QapiEventsMessage::Eof =>
                Err(io::Error::new(io::ErrorKind::UnexpectedEof, "unexpected EOF when syncing QMP connection")),
            #[cfg(feature = "qapi-qmp")]
            QapiEventsMessage::Event(event) =>
                Err(io::Error::new(io::ErrorKind::InvalidData, format!("unexpected QMP event: {:?}", event))),
        }));

        try_join!(sync, events).map(|((), ())| ())
    }
}


#[cfg(any(feature = "qapi-qmp", feature = "qapi-qga"))]
impl<W: AsyncWrite> QapiStream<W> {
    pub async fn execute<'a, C: Command + 'a>(self: &'a Self, command: C) -> io::Result<Result<C::Ok, qapi_spec::Error>> {
        await!(self.execute_(command, false))
    }

    pub async fn execute_oob<'a, C: Command + 'a>(self: &'a Self, command: C) -> io::Result<Result<C::Ok, qapi_spec::Error>> {
        /* TODO: should we assert C::ALLOW_OOB here and/or at the type level?
         * If oob isn't supported should we fall back to serial execution or error?
         */
        await!(self.execute_(command, true))
    }

    async fn execute_<'a, C: Command + 'a>(self: &'a Self, command: C, oob: bool) -> io::Result<Result<C::Ok, qapi_spec::Error>> {
        let (id, mut write, mut encoded) = if self.supports_oob {
            let id = self.next_oob_id();
            (
                Some(id),
                await!(self.write_lock.lock()),
                serde_json::to_vec(&qapi_spec::CommandSerializerRef::with_id(&command, id, oob))?,
            )
        } else {
            (
                None,
                await!(self.write_lock.lock()),
                serde_json::to_vec(&qapi_spec::CommandSerializerRef::new(&command, false))?,
            )
        };

        encoded.push(b'\n');
        await!(write.write_all(&encoded))?;

        if id.is_some() {
            // command mutex is unnecessary when protocol supports oob ids
            drop(write)
        }

        let (sender, receiver) = oneshot::channel();
        {
            let mut pending = await!(self.pending.lock());
            if let Some(prev) = pending.insert(id.unwrap_or(Default::default()), sender) {
                panic!("QAPI duplicate command id {:?}, this should not happen", prev);
            }
        }

        match await!(receiver) {
            Ok(Ok(res)) => Ok(Ok(serde::Deserialize::deserialize(&res)?)),
            Ok(Err(e)) => Ok(Err(e)),
            Err(_cancelled) => Err(io::Error::new(io::ErrorKind::UnexpectedEof, "QAPI stream disconnected")),
        }
    }

    pub async fn close(self) -> io::Result<()> {
        // forcefully stop event streams and spin() so the socket can be closed
        unimplemented!();
    }
}
