use std::pin::Pin;
use std::future::Future;
use std::sync::Arc;

use tokio::sync::Mutex;
use webrtc::rtcp::packet::Packet;
pub type OnPacketBatchFn = Box<
    dyn (FnMut(Vec<Box<dyn Packet + Send + Sync>>) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>)
        + Send
        + Sync,
>;

pub type OnCloseFn = Box<dyn (FnMut() -> Pin<Box<dyn Future<Output = ()> + Send>>) + Send + Sync>;

#[derive(Default)]
pub struct RTCPForwarder {
    ssrc: u32,
    on_packets_handler: Arc<Mutex<Option<OnPacketBatchFn>>>,
}

impl RTCPForwarder {
    pub fn new(ssrc: u32) -> Self {
        Self {
            ssrc,
            on_packets_handler: Arc::default(),
        }
    }

    /// Sets `on_packet` callback.
    /// This callback will be called from [Self::send_packets].
    pub async fn set_on_packets(&self, f: OnPacketBatchFn) {
        let mut on_packets = self.on_packets_handler.lock().await;
        *on_packets = Some(f);
    }

    pub async fn send_packets(&self, packets: Vec<Box<dyn Packet + Send + Sync>>) {
        let mut handler = self.on_packets_handler.lock().await;
        if let Some(f) = handler.as_mut() {
            f(packets).await;
        } else {
            trace!("[ssrc={}] No callback set. {} RTCP packets lost.", self.ssrc, packets.len());
        }
    }
}