use std::{collections::BTreeMap, sync::Arc, time::{SystemTime, UNIX_EPOCH}};

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
#[derive(Default)]
struct Sequencer {
    init: bool,
    max: i32,
    seq: BTreeMap<i32, PacketMeta>,
    step: i32,
    head_sn: u16,
    start_time: u128,
}

impl Sequencer {
    pub fn new(max_track: i32) -> Self {
        Self {
            max: max_track,
            seq: BTreeMap::new(),
            start_time: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis(),
            ..Default::default()
        }
    }
}

#[derive(Default)]
pub struct AtomicSequencer {
    sequencer: Arc<Mutex<Sequencer>>,
}



impl AtomicSequencer {
    pub fn new(max_track: i32) -> Self {
        Self {
            sequencer: Arc::new(Mutex::new(Sequencer::new(max_track))),
        }
    }

    /// Inserts a new RTP packet into the sequencer and returns the next ordered packet (if available).
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
    ) -> Option<PacketMeta> {
        let mut sequencer = self.sequencer.lock().await;

        // Is the first packet?
        if !sequencer.init {
            sequencer.head_sn = off_sn;
            sequencer.init = true;
        }

        if head {
            let inc = off_sn.wrapping_sub(sequencer.head_sn) as i16;

            if inc > 0 {
                sequencer.step = (sequencer.step + inc as i32) % sequencer.max;
            }

            sequencer.head_sn = off_sn;
        } else {
            let delta = sequencer.head_sn.wrapping_sub(off_sn) as i16;
            let step = sequencer.step - delta as i32;
            if step < 0 && -step >= sequencer.max {
                return None;
            }
        }

        let cur_step = sequencer.step;

        sequencer.seq.insert(
            cur_step,
            PacketMeta {
                source_seq_no: sn,
                target_seq_no: off_sn,
                timestamp,
                layer,
                ..Default::default()
            },
        );

        sequencer.step += 1;

        if sequencer.step >= sequencer.max {
            sequencer.step = 0;
        }

        if let Some(data) = sequencer.seq.get(&sequencer.step) {
            Some(data.clone())
        } else {
            trace!("sequencer step: {}", sequencer.step);
            None
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
    pub async fn get_seq_no_pairs(&self, seq_nos: &[u16]) -> Vec<PacketMeta> {
        let mut sequencer = self.sequencer.lock().await;

        let mut meta: Vec<PacketMeta> = Vec::new();

        let now_as_millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let ref_time = now_as_millis - sequencer.start_time;

        for sn in seq_nos {
            let delta = (sequencer.head_sn.wrapping_sub(*sn)) as i32;
            let mut step = sequencer.step - delta - 1;

            if step < 0 {
                if -step >= sequencer.max {
                    continue;
                }

                step += sequencer.max;
            }

            let seq = sequencer.seq.get_mut(&step);
            if let Some(seq) = seq {
                if seq.target_seq_no == *sn
                    && (seq.last_nack == 0 || ref_time - seq.last_nack > IGNORE_RETRANSMISSION as u128)
                {
                    seq.last_nack = ref_time;
                    meta.push(seq.clone());
                }
            }
        }

        meta
    }
}