use std::pin::Pin;
use std::future::Future;
use std::sync::Arc;

use tokio::sync::Mutex;
use webrtc::rtcp::packet::{Packet, unmarshal};
use super::error::Result;

pub type OnPacketBatchFn = Box<
    dyn (FnMut(Vec<Box<dyn Packet + Send + Sync>>) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>)
        + Send
        + Sync,
>;

pub type OnCloseFn = Box<dyn (FnMut() -> Pin<Box<dyn Future<Output = ()> + Send>>) + Send + Sync>;

#[derive(Default)]
pub struct RTCPReader {
    on_packets_handler: Arc<Mutex<Option<OnPacketBatchFn>>>,
}

impl RTCPReader {
    pub fn new() -> Self {
        Self {
            on_packets_handler: Arc::default(),
        }
    }

    /// Sets `on_packet` callback.
    /// This callback will be called from [Self::write].
    pub async fn set_on_packets(&self, f: OnPacketBatchFn) {
        let mut on_packets = self.on_packets_handler.lock().await;
        *on_packets = Some(f);
    }

    /// Unmarshal given raw data into RTCP packets. Every batch of packets calls `on_packet` handler.
    /// # Returns
    /// - `Ok(usize)` - Length of packets resulted from unmarshal
    /// - `Err(rtcp::Error)` - Error trying to read the data.
    pub async fn write(&self, data: &[u8]) -> Result<usize> {
        // Use a mutable copy
        let mut buf = &data[..];
        let packets = unmarshal(&mut buf)?;
        let len = packets.len();
        let mut handler = self.on_packets_handler.lock().await;
        if let Some(f) = handler.as_mut() {
            f(packets).await;
        }
        Ok(len)
    }
}