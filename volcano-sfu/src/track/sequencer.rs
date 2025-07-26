use std::{collections::BTreeMap, sync::{atomic::{AtomicU16, Ordering}, Arc}, time::Instant};

use tokio::sync::Mutex;
const IGNORE_RETRANSMISSION: u8 = 100;
#[derive(Default, Clone)]
pub struct PacketMeta {
    // Original sequence number from stream.
    // The original sequence number is used to find the original
    // packet from publisher
    pub source_seq_no: u16,
    // Modified sequence number after offset.
    // This sequence number is used for the associated
    // down track, is modified according the offsets, and
    // must not be shared
    pub target_seq_no: u16,
    // Modified timestamp for current associated
    // down track.
    pub timestamp: u32,
    // The last time this packet was nack requested.
    // Sometimes clients request the same packet more than once, so keep
    // track of the requested packets helps to avoid writing multiple times
    // the same packet.
    // The resolution is 1 ms counting after the sequencer start time.
    last_nack: u128,
    // Spatial layer of packet
    pub layer: u8,
    // Information that differs depending the codec
    misc: u32,
}

impl PacketMeta {
    pub fn set_vp8_payload_meta(&mut self, tlz0_idx: u8, pic_id: u16) {
        self.misc = ((tlz0_idx as u32) << 16) | (pic_id as u32);
    }
    pub fn get_vp8_payload_meta(&self) -> (u8, u16) {
        ((self.misc >> 16) as u8, self.misc as u16)
    }
}

struct Sequencer {
    init: bool,
    max: u16,
    seq: BTreeMap<u16, PacketMeta>,
    step: u16,
    head_sn: u16,
    start_time: Instant,
}

impl Sequencer {
    pub fn new(max_track: u16) -> Self {
        assert!(max_track > 0, "Sequencer max_track must be > 0");
        Self {
            max: max_track,
            seq: BTreeMap::new(),
            start_time: Instant::now(),
            init: false,
            step: 0,
            head_sn: 0,
        }
    }
}

pub struct AtomicSequencer {
    inner: Arc<Mutex<Sequencer>>,
    next: AtomicU16,
}

impl AtomicSequencer {
    pub fn new(max_track: u32) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Sequencer::new(max_track as u16))),
            next: AtomicU16::default(),
        }
    }

    /// Returns actual count and increments for subsequent calls.
    pub fn next_sn(&self) -> u16 {
        self.next.fetch_add(1, Ordering::SeqCst)
    }

    /// Inserts a new RTP packet into the sequencer.
    ///
    /// This method handles sequence number tracking, reordering logic, and step management to ensure
    /// packets are emitted in the correct order, even when received out of sequence.
    ///
    /// # Arguments
    ///
    /// * `sn` - Original sequence number of the incoming packet.
    /// * `off_sn` - Offset sequence number used for synchronization.
    /// * `timestamp` - RTP timestamp of the packet.
    /// * `layer` - Spatial/temporal layer index.
    /// * `head` - If true, resets or updates the head reference for sequencing.
    ///
    pub async fn push(
        &self,
        sn: u16,
        off_sn: u16,
        timestamp: u32,
        layer: u8,
        head: bool,
    ) {
        let mut inner = self.inner.lock().await;

        // Is the first packet?
        if !inner.init {
            inner.head_sn = off_sn;
            inner.init = true;
        }

        if head {
            trace!("push (head): Packet #{0}: sn={sn}", inner.step);
            let distance = off_sn.wrapping_sub(inner.head_sn);
            inner.step = inner.step.wrapping_add(distance).rem_euclid(inner.max);
            inner.head_sn = off_sn;
        }

        //let index = inner.step % inner.max;

        let target_seq_no = self.next_sn() % inner.max;
        
        let meta = PacketMeta {
            source_seq_no: sn,
            target_seq_no,
            timestamp,
            layer,
            ..Default::default()
        };
        // Insert by sequence_number
        inner.seq.insert(
            target_seq_no,
            meta,
        );

        // Safe, step < max
        inner.step += 1;

        // Reset on max reached
        if inner.step >= inner.max {
            inner.step = 0;
        }
    }

    /// Gets a list of packets matching the requested sequence numbers for retransmission,
    /// filtering out recently retransmitted packets to avoid flooding.
    ///
    /// # Arguments
    /// * `seq_nos` - Slice of sequence numbers to look up for retransmission.
    ///
    /// # Returns
    /// A vector of `PacketMeta` objects ready to be retransmitted.
    ///
    /// # Example
    ///
    /// ```no_run
    /// let packets = downtrack.get_seq_no_pairs(&[1234, 5678]).await;
    /// for packet in packets {
    ///     send_rtcp_nack(&packet).await?;
    /// }
    /// ```
    pub async fn get_seq_no_pairs(&self, target_snos: &[u16]) -> Vec<PacketMeta> {
        let mut inner = self.inner.lock().await;

        let mut meta: Vec<PacketMeta> = Vec::new();

        let elapsed = inner.start_time.elapsed().as_millis();

        for target in target_snos {
            if let Some(pkt) = inner.seq.get_mut(target) {
                if &pkt.target_seq_no == target
                    //&& (pkt.last_nack == 0
                     //   || elapsed.saturating_sub(pkt.last_nack) > IGNORE_RETRANSMISSION as u128)
                {
                    let elapsed = if pkt.last_nack == 0 {
                        (IGNORE_RETRANSMISSION + 1) as u128
                    } else {
                        elapsed.saturating_sub(pkt.last_nack)
                    };
                    if elapsed > IGNORE_RETRANSMISSION as u128 {
                        pkt.last_nack = elapsed;
                        meta.push(pkt.clone());
                    }
                }
            }
        }

        meta
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn test_sequencer_new() {
        let sequencer = Sequencer::new(100);
        assert_eq!(sequencer.max, 100);
        assert!(sequencer.seq.is_empty());
        assert_eq!(sequencer.step, 0);
        assert_eq!(sequencer.head_sn, 0);
        // Sequencer had not received a packet
        assert!(!sequencer.init);
    }

    #[test]
    #[should_panic = "max_track must be > 0"]
    fn test_sequencer_new_invalid() {
        Sequencer::new(0);
    }

    #[tokio::test]
    async fn test_atomic_sequencer_push_packets() {
        let sequencer = AtomicSequencer::new(10);

        for i in 0..5 {
            sequencer.push(i + 100, i + 100, (i as u32 * 960) + 1_000, 0, false).await;
        }
        
        let packets = sequencer.get_seq_no_pairs(&[0, 1, 2, 3, 4]).await;
        assert_eq!(packets.len(), 5);
    }

    #[tokio::test]
    async fn test_fetch_retransmition() {
        let sequencer = AtomicSequencer::new(10);

        sequencer.push(100, 100, 1_000, 0, true).await;
        let search_1 = sequencer.get_seq_no_pairs(&[0]).await;
        assert_eq!(search_1.len(), 1);

        let search_2 = sequencer.get_seq_no_pairs(&[0]).await;
        assert!(search_2.is_empty(), "packet should be ignored to prevent flood");

        tokio::time::sleep(Duration::from_millis(200)).await;

        let search_3 = sequencer.get_seq_no_pairs(&[0]).await;
        assert_eq!(search_3.len(), 1, "packet should be returned");
    }
}
