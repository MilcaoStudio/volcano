use std::sync::Arc;

use std::fmt::Debug;
use futures::{
    stream::{SplitSink, SplitStream},
    SinkExt,
};
use serde::Serialize;
use tokio::{net::TcpStream, sync::Mutex};
use tokio_tungstenite::{tungstenite::Message, WebSocketStream};

type Sink = SplitSink<WebSocketStream<TcpStream>, Message>;

/// Sink side of the WebSocket stream behind a Mutex for distributed writing
#[derive(Clone)]
pub struct Sender {
    writer: Arc<Mutex<Sink>>,
}

impl Sender {
    /// Create a new Sender
    pub fn new(sink: Sink) -> Self {
        Sender {
            writer: Arc::new(Mutex::new(sink)),
        }
    }

    /// Send a packet through the WebSocket
    pub async fn send<Packet>(&self, packet: Packet) -> anyhow::Result<()>
    where Packet: Debug + Serialize {
        debug!("S->C: {:?}", packet);
        self.writer
            .lock()
            .await
            .send(Message::Text(serde_json::to_string(&packet)?))
            .await.map_err(Into::into)
    }

    /// Closes socket stream with code 1005
    pub async fn close(&self) -> anyhow::Result<()> {
        self.writer.lock().await.close().await.map_err(Into::into)
    }
}

/// Pair of sink and stream
pub type ReadWritePair = (SplitStream<WebSocketStream<TcpStream>>, Sender);