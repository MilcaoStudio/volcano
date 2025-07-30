use std::sync::Arc;

use futures::{
    stream::{SplitSink, SplitStream},
    SinkExt,
};
use tokio::{net::TcpStream, sync::Mutex};
use tokio_tungstenite::{tungstenite::Message, WebSocketStream};

use super::packets::PacketS2C;

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
    pub async fn send(&self, packet: PacketS2C) -> anyhow::Result<()> {
        debug!("S->C: {:?}", packet);
        self.writer
            .lock()
            .await
            .send(Message::Text(serde_json::to_string(&packet)?))
            .await.map_err(Into::into)
    }
}

/// Pair of sink and stream
pub type ReadWritePair = (SplitStream<WebSocketStream<TcpStream>>, Sender);