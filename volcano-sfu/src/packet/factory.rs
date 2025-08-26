use super::AtomicBuffer;
use super::rtcp::RTCPForwarder;
use std::sync::Arc;
use dashmap::DashMap;

#[derive(Default)]
pub struct Factory {
    pub rtp_buffers: DashMap<u32, Arc<AtomicBuffer>>,
    pub rtcp_readers: DashMap<u32, Arc<RTCPForwarder>>,
}

#[derive(Default)]
pub struct AtomicFactory {
    factory: Factory,
}

impl AtomicFactory {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get_or_new_rtcp_buffer(&self, ssrc: u32) -> Arc<RTCPForwarder> {
        let entry = self.factory.rtcp_readers.entry(ssrc);
        entry.or_insert(Arc::new(
            RTCPForwarder::new(ssrc)
        )).clone()
    }

    pub fn get_or_new_buffer(&self, ssrc: u32) -> Arc<AtomicBuffer> {
        let entry = self.factory.rtp_buffers.entry(ssrc);
        entry.or_insert(Arc::new(
            AtomicBuffer::new(ssrc)
        )).clone()
    }

    pub fn get_rtp_buffer(&self, ssrc: u32) -> Option<Arc<AtomicBuffer>> {
        self.factory.rtp_buffers.get(&ssrc).map(|b|b.clone())
    }

    pub fn get_rtcp_buffer(&self, ssrc: u32) -> Option<Arc<RTCPForwarder>> {
        self.factory.rtcp_readers.get(&ssrc).map(|b| b.clone())
    }
}
