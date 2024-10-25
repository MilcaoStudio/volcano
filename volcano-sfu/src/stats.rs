use std::sync::{atomic::{AtomicU64, Ordering}, Arc};

use tokio::sync::Mutex;

use crate::buffer::buffer::{AtomicBuffer, Stats};


#[allow(dead_code)]
#[derive(Default)]
pub struct StreamStats {
    has_stas: bool,
    last_stats: Stats,
    diff_stats: Stats,
}

#[allow(dead_code)]
pub struct Stream {
    buffer: Arc<AtomicBuffer>,

    cname: Arc<Mutex<String>>,
    drift_in_millis: AtomicU64,
    stats: Arc<Mutex<StreamStats>>,
}

impl Stream {
    pub fn new(buffer: Arc<AtomicBuffer>) -> Self {
        Self {
            buffer,
            cname: Arc::new(Mutex::new(String::default())),
            drift_in_millis: AtomicU64::default(),
            stats: Arc::new(Mutex::new(StreamStats::default())),
        }
    }

    #[allow(dead_code)]
    async fn get_cname(&mut self) -> String {
        let cname = self.cname.lock().await;
        cname.clone()
    }

    pub async fn set_cname(&mut self, val: String) {
        let mut cname = self.cname.lock().await;
        *cname = val;
    }

    #[allow(dead_code)]
    fn set_drift_in_millis(&mut self, val: u64) {
        self.drift_in_millis.store(val, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    fn get_drift_in_millis(&self) -> u64 {
        self.drift_in_millis.load(Ordering::Relaxed)
    }

    #[allow(dead_code)]
    async fn update_stats(&mut self, stats: Stats) -> (bool, Stats) {
        let mut cur_stats = self.stats.lock().await;

        let mut has_status = false;

        if cur_stats.has_stas {
            cur_stats.diff_stats.last_expected =
                stats.last_expected - cur_stats.last_stats.last_expected;

            cur_stats.diff_stats.last_expected =
                stats.last_received - cur_stats.last_stats.last_received;

            cur_stats.diff_stats.packet_count =
                stats.packet_count - cur_stats.last_stats.packet_count;

            cur_stats.diff_stats.total_byte = stats.total_byte - cur_stats.last_stats.total_byte;

            has_status = true
        }

        cur_stats.last_stats = stats;
        cur_stats.has_stas = true;

        (has_status, cur_stats.diff_stats.clone())
    }

}