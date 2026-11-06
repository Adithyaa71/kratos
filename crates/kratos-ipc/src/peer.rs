//! A client connection to the broker, for any role.
//!
//! Lives here rather than in a binary so the MCP shim, the settings window,
//! the Python backend harness and the integration tests all share one
//! handshake. A handshake implemented twice is a handshake that diverges.
//!
//! Sequential by design: one request in flight at a time, correlated by id.
//! Events that arrive mid-call are buffered rather than mistaken for the
//! reply, which matters because the broker broadcasts a kill event to every
//! peer and it can land at any moment.

use std::collections::VecDeque;
use std::io;

use kratos_protocol::{codes, ops, Frame, Role, WireError};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite, ReadHalf, WriteHalf};
use tokio::net::windows::named_pipe::NamedPipeClient;

use crate::{FrameReader, FrameWriter};

pub struct Peer<S> {
    reader: FrameReader<ReadHalf<S>>,
    writer: FrameWriter<WriteHalf<S>>,
    /// Events received while waiting for a reply, oldest first.
    events: VecDeque<Frame>,
    pub peer_id: String,
    pub broker: String,
    pub decider: String,
    pub kill_engaged: Vec<String>,
    /// The whole hello reply, kept verbatim.
    ///
    /// The named fields above are the ones every caller needs; this is for the
    /// ones a later protocol addition brings -- the `lane` object, say -- so a
    /// new reply field is readable without widening this struct and every
    /// caller with it.
    pub ack: Value,
}

/// The common case: a peer on a real named pipe.
pub type PipePeer = Peer<NamedPipeClient>;

impl<S> Peer<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    /// Wrap an already-connected stream and complete the handshake.
    ///
    /// Generic over the stream so a test can drive a peer over an in-memory
    /// duplex without a pipe, and so the same code path is exercised either
    /// way.
    pub async fn handshake(stream: S, hello: kratos_protocol::Hello) -> io::Result<Self> {
        let (reader, writer) = crate::split(stream);
        let mut peer = Peer {
            reader,
            writer,
            events: VecDeque::new(),
            peer_id: String::new(),
            broker: String::new(),
            decider: String::new(),
            kill_engaged: Vec::new(),
            ack: Value::Null,
        };
        // Serialised from the typed `Hello` rather than assembled by hand, so
        // a field added to the struct reaches the wire without a second place
        // to remember to change. The hand-built object silently omitted
        // `instance` for exactly that reason.
        let payload = serde_json::to_value(&hello)
            .map_err(|e| io::Error::other(format!("unserialisable hello: {e}")))?;
        let ack = peer
            .call(ops::HELLO, payload)
            .await
            .map_err(|e| io::Error::other(format!("handshake refused: {e}")))?;
        peer.peer_id = ack["peer_id"].as_str().unwrap_or_default().to_string();
        peer.broker = ack["broker"].as_str().unwrap_or_default().to_string();
        peer.decider = ack["decider"].as_str().unwrap_or_default().to_string();
        peer.kill_engaged = string_list(&ack["kill_engaged"]);
        peer.ack = ack;
        Ok(peer)
    }

    /// Send a request and wait for its reply.
    pub async fn call(&mut self, op: &str, payload: Value) -> Result<Value, WireError> {
        let id = crate::next_id("c");
        self.writer.send(&Frame::req(&id, op, payload)).await.map_err(|e| {
            WireError::new(codes::INTERNAL, format!("could not send {op}: {e}"))
        })?;

        loop {
            match self.reader.next().await {
                Ok(Some(frame)) => {
                    if matches!(frame.body, kratos_protocol::Body::Evt { .. }) {
                        if frame.op() == Some(ops::EVT_KILL) {
                            self.kill_engaged = string_list(&frame.payload()["engaged"]);
                        }
                        // Kept, not dropped: a caller polling for events must
                        // still see one that arrived during a call.
                        self.events.push_back(frame);
                        if self.events.len() > 64 {
                            self.events.pop_front();
                        }
                        continue;
                    }
                    if frame.id() != Some(id.as_str()) {
                        continue; // a reply to something else; ignore
                    }
                    return if frame.is_ok() {
                        Ok(frame.payload().clone())
                    } else {
                        Err(frame
                            .wire_error()
                            .cloned()
                            .unwrap_or_else(|| WireError::new(codes::INTERNAL, "unexplained refusal")))
                    };
                }
                Ok(None) => {
                    return Err(WireError::new(codes::INTERNAL, "the broker closed the connection"))
                }
                Err(e) if e.is_fatal() => {
                    return Err(WireError::new(codes::INTERNAL, format!("connection lost: {e}")))
                }
                Err(_) => {} // one unparseable line; keep waiting
            }
        }
    }

    /// Reply to a request the broker sent us. Backends only.
    pub async fn reply(&mut self, id: &str, payload: Value) -> io::Result<()> {
        self.writer.send(&Frame::ok(id, payload)).await
    }

    pub async fn reply_error(&mut self, id: &str, error: WireError) -> io::Result<()> {
        self.writer.send(&Frame::err(id, error)).await
    }

    /// The next frame of any kind, including requests from the broker.
    pub async fn next_frame(&mut self) -> Result<Option<Frame>, crate::RecvError> {
        if let Some(evt) = self.events.pop_front() {
            return Ok(Some(evt));
        }
        self.reader.next().await
    }

    /// An event that arrived earlier, if any.
    pub fn take_event(&mut self) -> Option<Frame> {
        self.events.pop_front()
    }
}

impl Peer<NamedPipeClient> {
    /// Connect, starting the broker if nothing is listening. This is the lazy
    /// start: nothing runs until something needs it.
    pub async fn connect_or_start(base: &str, role: Role, name: &str, build: &str) -> io::Result<Self> {
        let exe = crate::broker_exe()?;
        let conn = crate::connect_or_start(base, &exe).await?;
        Peer::handshake(conn, kratos_protocol::Hello::new(role, name, build)).await
    }

    /// Connect to an already-running broker, never starting one.
    pub async fn attach(base: &str, role: Role, name: &str, build: &str) -> io::Result<Self> {
        let conn = crate::connect(base).await?;
        Peer::handshake(conn, kratos_protocol::Hello::new(role, name, build)).await
    }

    /// Connect with a hello of the caller's own making.
    ///
    /// The route for anything the convenience constructors do not cover: a
    /// backend that is one instance among several, or an agent that knows its
    /// harness and agent names.
    pub async fn attach_as(base: &str, hello: kratos_protocol::Hello) -> io::Result<Self> {
        let conn = crate::connect(base).await?;
        Peer::handshake(conn, hello).await
    }

    /// Connect as a perception/actuation backend.
    pub async fn attach_backend(
        base: &str,
        name: &str,
        build: &str,
        source: &str,
        handles: Vec<String>,
    ) -> io::Result<Self> {
        let conn = crate::connect(base).await?;
        Peer::handshake(conn, kratos_protocol::Hello::backend(name, build, source, handles)).await
    }

    /// Connect as one instance of a backend source.
    ///
    /// `instance` of `None` is identical to [`attach_backend`]: a source with
    /// no instance is the only instance of itself, which is what `uia`, `ocr`
    /// and `pixel` are.
    pub async fn attach_backend_instance(
        base: &str,
        name: &str,
        build: &str,
        source: &str,
        handles: Vec<String>,
        instance: Option<kratos_protocol::Instance>,
    ) -> io::Result<Self> {
        let conn = crate::connect(base).await?;
        let hello =
            kratos_protocol::Hello::backend(name, build, source, handles).with_instance(instance);
        Peer::handshake(conn, hello).await
    }
}

fn string_list(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default()
}
