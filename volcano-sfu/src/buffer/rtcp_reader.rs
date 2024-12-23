use std::pin::Pin;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::Mutex;

use super::error::{BufferError, Result};

pub type OnPacketFn = Box<
    dyn (FnMut(Vec<u8>) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'static>>)
        + Send
        + Sync,
>;
pub type OnCloseFn = Box<dyn (FnMut() -> Pin<Box<dyn Future<Output = ()> + Send>>) + Send + Sync>;

pub struct RTCPReader {
    #[allow(dead_code)]
    ssrc: u32,
    closed: AtomicBool,
    on_packet_handler: Arc<Mutex<Option<OnPacketFn>>>,
    on_close_handler: Arc<Mutex<Option<OnCloseFn>>>,
}

impl RTCPReader {
    pub fn new(ssrc: u32) -> Self {
        Self {
            ssrc,
            closed: AtomicBool::new(false),
            on_packet_handler: Arc::default(),
            on_close_handler: Arc::default(),
        }
    }

    pub async fn on_close(&mut self, f: OnCloseFn) {
        let mut on_close = self.on_close_handler.lock().await;
        *on_close = Some(f);
    }

    pub async fn register_on_packet(&mut self, f: OnPacketFn) {
        let mut on_packet = self.on_packet_handler.lock().await;
        *on_packet = Some(f);
    }

    pub fn read(&mut self, _buff: &mut [u8]) -> Result<usize> {
        Ok(0)
    }
    pub async fn write(&mut self, p: Vec<u8>) -> Result<u32> {
        if self.closed.load(Ordering::Relaxed) {
            return Err(BufferError::ErrIOEof);
        }

        info!("Calling on_packet_handler");

        let mut handler = self.on_packet_handler.lock().await;
        if let Some(h) = &mut *handler {
            h(p).await?;
        }

        Ok(9)
    }
}