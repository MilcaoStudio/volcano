use super::AtomicBuffer;
use super::rtcp::RTCPForwarder;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Default)]
pub struct Factory {
    pub rtp_buffers: HashMap<u32, Arc<AtomicBuffer>>,
    pub rtcp_readers: HashMap<u32, Arc<RTCPForwarder>>,
}

#[derive(Default)]
pub struct AtomicFactory {
    factory: Arc<Mutex<Factory>>,
}

impl AtomicFactory {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn get_or_new_rtcp_buffer(&self, ssrc: u32) -> Arc<RTCPForwarder> {
        let mut factory = self.factory.lock().await;
        let entry = factory.rtcp_readers.entry(ssrc);
        entry.or_insert(Arc::new(
            RTCPForwarder::new()
        )).clone()
    }

    pub async fn get_or_new_buffer(&self, ssrc: u32) -> Arc<AtomicBuffer> {
        let mut factory = self.factory.lock().await;
        let entry = factory.rtp_buffers.entry(ssrc);
        entry.or_insert(Arc::new(
            AtomicBuffer::new(ssrc)
        )).clone()
    }
}
