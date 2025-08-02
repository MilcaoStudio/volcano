use std::{sync::Arc};

use dashmap::{DashMap, DashSet};

#[derive(Default, Clone, Debug)]
struct AudioStream {
    id: String,
    sum: i32,
    total: i32,
}

/// Audio observer for audio activity detection.
/// 
/// # Examples
/// ```
/// use std::sync::Arc;
/// use tokio::sync::Mutex;
/// use tokio::time::sleep;
/// use tokio::runtime::Runtime;
/// use std::time::Duration;
/// use volcano_sfu::track::audio_observer::AudioObserver;
/// 
/// let rt = Runtime::new().unwrap();
/// let observer = Arc::new(Mutex::new(AudioObserver::new(70, 100, 50)));
/// let observer2 = observer.clone();
/// 
/// rt.spawn(async move {
///     let mut i = 0u8;
///     observer2.lock().await.add_stream("stream1".to_owned()).await;
///     while i < 10 {
///         observer2.lock().await.observe("stream1", 50).await;
///     }
/// });
/// 
/// rt.block_on(async move {
///     let mut i = 0u8;
///     while i < 5 {
///         let mut observer = observer.lock().await;
///         let streams = observer.calc().await;
///         if let Some(streams) = streams {
///             assert_eq!(streams.len(), 1);
///             assert_eq!(streams[0], "stream1");
///         }
///         sleep(Duration::from_millis(observer.interval as u64)).await;
///         i += 1;
///     }
/// });
/// ```
#[derive(Default, Clone, Debug)]
pub struct AudioObserver {
    streams: DashMap<String, AudioStream>,
    /// Expected **total audio power** an audio stream should have to be considered active.
    pub expected: i32,
    /// In each observation interval, if the audio level is lower than `threshold`, audio power is accumulated.
    pub threshold: u8,
    /// Interval in milliseconds between each calculation of audio activity.
    pub interval: i32,
    previous: Arc<DashSet<String>>,
}

impl AudioObserver {
    /// Creates an audio observer with threshold lower than 128, an interval in milliseconds, and a filter from 0 to 100.
    pub fn new(threshold_parameter: u8, interval_parameter: i32, filter_parameter: i32) -> Self {
        let threshold: u8 = threshold_parameter.clamp(0, 127);
        let filter: i32 = filter_parameter.clamp(0, 100);
        
        Self {
            threshold,
            interval: interval_parameter,
            expected: interval_parameter * filter / 2000,
            ..Default::default()
        }
    }

    pub async fn add_stream(&self, stream_id: String) {
        self.streams.insert(stream_id.clone(), AudioStream {
            id: stream_id,
            ..Default::default()
        });
    }

    pub async fn remove_stream(&self, stream_id: &str) {
        debug!("Remove stream {}", stream_id);
        self.streams.remove(stream_id);
    }

    /// Observes whether `d_bov` is higher than threshold for target stream, then it should be ignored.
    /// 
    /// If `d_bov` is lower or equal than treshold, it sums `d_bov` into target stream.
    pub async fn observe(&self, stream_id: &str, d_bov: u8) {
        if let Some(mut stream) = self.streams.get_mut(stream_id) {
            // Active voice level should be lower than threshold
            if d_bov <= self.threshold {
                stream.sum += d_bov as i32;
                stream.total += 1;
            }
        }
    }

    /// Sorts current streams vector by total, and secondly by sum.
    /// 
    /// Filters streams which total is equal or higher than `expected`, the sum and total from selected streams are reset.
    /// # Returns
    /// Vector of stream ids from selected streams, or None if the vector could be empty.
    pub async fn calc(&mut self) -> Option<Vec<String>> {
        let current_ids = Arc::new(self.streams.iter().map(|s| s.key().clone()).collect::<DashSet<_>>());

        let mut stream_ids = Vec::new();

        for mut stream in self.streams.iter_mut() {
            if stream.total >= self.expected {
                debug!("[stream {}] {}/{} (acceptable)", stream.id, stream.total, self.expected);
                stream_ids.push(stream.id.clone());
            }

            stream.total = 0;
            stream.sum = 0;
        }

        if current_ids.len() == self.previous.len() &&
            self.previous.iter().all(|k| current_ids.contains(k.key())) {
            return None;
        }

        let _ = std::mem::replace(&mut self.previous, current_ids);

        Some(stream_ids)
    }

    /// Returns true if there are no streams.
    pub async fn is_empty(&self) -> bool {
        self.streams.is_empty()
    }
}
