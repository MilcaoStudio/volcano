use std::pin::Pin;
use std::future::Future;
use std::sync::Arc;

use tokio::sync::Mutex;

use super::error::Result;

pub type OnPacketFn = Box<
    dyn (FnMut(Vec<u8>) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'static>>)
        + Send
        + Sync,
>;
pub type OnCloseFn = Box<dyn (FnMut() -> Pin<Box<dyn Future<Output = ()> + Send>>) + Send + Sync>;

pub struct RTCPReader {
    on_packet_handler: Arc<Mutex<Option<OnPacketFn>>>,
}

impl RTCPReader {
    pub fn new(_ssrc: u32) -> Self {
        Self {
            on_packet_handler: Arc::default(),
        }
    }

    /// Sets `on_packet` callback.
    /// This callback will be called from #write.
    pub async fn register_on_packet(&mut self, f: OnPacketFn) {
        let mut on_packet = self.on_packet_handler.lock().await;
        *on_packet = Some(f);
    }

    /// Calls `on_packet` with the given data.
    pub async fn write(&mut self, p: Vec<u8>) -> Result<u32> {

        info!("Calling on_packet_handler");

        let mut handler = self.on_packet_handler.lock().await;
        if let Some(h) = &mut *handler {
            h(p).await?;
        }

        Ok(9)
    }
}