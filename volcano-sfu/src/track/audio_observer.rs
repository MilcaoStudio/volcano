use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Default, Clone, Debug)]
pub struct AudioStream {
    id: String,
    sum: i32,
    total: i32,
}
#[derive(Default, Clone, Debug)]
pub struct AudioObserver {
    streams: Arc<Mutex<Vec<AudioStream>>>,
    expected: i32,
    threshold: u8,
    previous: Vec<String>,
}

impl AudioObserver {
    /// Creates an audio observer with threshold lower than 128, an interval, and a filter from 0 to 100.
    /// ## Example
    /// ```
    /// let observer = AudioObserver::new(100, 80, 50);
    /// ```
    pub fn new(threshold_parameter: u8, interval_parameter: i32, filter_parameter: i32) -> Self {
        let mut threshold: u8 = threshold_parameter;
        if threshold > 127 {
            threshold = 127;
        }
        let mut filter: i32 = filter_parameter;
        if filter < 0 {
            filter = 0;
        }
        if filter > 100 {
            filter = 100;
        }
        Self {
            threshold,
            expected: interval_parameter * filter / 2000,
            ..Default::default()
        }
    }

    pub async fn add_stream(&mut self, stream_id: String) {
        self.streams.lock().await.push(AudioStream {
            id: stream_id,
            ..Default::default()
        })
    }

    pub async fn remove_stream(&mut self, stream_id: &str) {
        let mut streams = self.streams.lock().await;
        streams.retain(|stream| !stream.id.eq(stream_id));
    }

    /// Observes whether d_bov is higher than threshold for target stream.
    /// If d_bov is lower or equal than treshold, it sums d_bov into target stream.
    pub async fn observe(&self, stream_id: &str, d_bov: u8) {
        let mut streams = self.streams.lock().await;
        
        let target = streams.iter_mut()
            .find(|stream| stream.id.eq(stream_id));
        
        if let Some(stream) = target {
            if d_bov <= self.threshold {
                stream.sum += d_bov as i32;
                stream.total += 1;
            }
        }
    }

    /// Sorts current streams vector by total, and secondly by sum.
    /// Filters streams which total is equal or higher than expected value, the sum and total from selected streams are reset.
    /// Returns vector of stream ids from selected streams, or None if the vector could be empty.
    pub async fn calc(&mut self) -> Option<Vec<String>> {
        let mut streams = self.streams.lock().await;

        streams.sort_by(|a, b| {
            if b.total != a.total {
                return b.total.cmp(&a.total);
            }
            b.sum.cmp(&a.sum)
        });

        let mut stream_ids = Vec::new();

        for stream in streams.iter_mut() {
            if stream.total >= self.expected {
                stream_ids.push(stream.id.clone());
            }

            stream.total = 0;
            stream.sum = 0;
        }

        if self.previous.len() == stream_ids.len() {
            for idx in 0..self.previous.len() {
                if self.previous[idx] != stream_ids[idx] {
                    self.previous = stream_ids.clone();

                    return Some(stream_ids);
                }
            }
            return None;
        }
        self.previous = stream_ids.clone();

        Some(stream_ids)
    }
}
