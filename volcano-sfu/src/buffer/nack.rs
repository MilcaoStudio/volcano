use webrtc::rtcp::transport_feedbacks::transport_layer_nack::NackPair;

pub const MAX_NACK_TIMES: u8 = 3;
#[derive(Debug, Eq, PartialEq, Default, Clone)]
pub struct Nack {
    seq_number: u32,
    nackd: u8,
}

impl Nack {
    pub fn new(seq_number: u32, nackd: u8) -> Self {
        Self { seq_number, nackd }
    }
}

#[derive(Debug, Eq, PartialEq, Default, Clone)]
pub struct NackQueue {
    nacks: Vec<Nack>,
    key_frame_seq_number: u32,
}

impl NackQueue {
    pub fn new() -> Self {
        Self {
            nacks: Vec::new(),
            key_frame_seq_number: 0,
        }
    }
    pub fn push(&mut self, sn: u32) {
        /*find the specified nack ele according to the sequence number*/
        let rv = self.nacks.binary_search_by_key(
            &sn,
            |&Nack {
                 seq_number,
                 nackd: _nackd,
             }| seq_number,
        );

        let insert_index: usize = match rv {
            Ok(_) => {
                /*exists then return*/
                return;
            }
            Err(index) => {
                /*not exists then insert the new nack*/
                index
            }
        };

        let nack = Nack::new(sn, 0);
        self.nacks.insert(insert_index, nack);
    }

    pub fn remove(&mut self, sn: u32) -> Option<Nack> {
        if let Ok(index) = self.nacks.binary_search_by_key(
            &sn,
            |&Nack {
                 seq_number,
                 nackd: _nackd,
             }| seq_number,
        ) {
            return Some(self.nacks.remove(index));
        }

        None
    }
    // wireshark package:
    // 0x2450 ffff 2461 ffff
    // RTCP Transport Feedback NACK PID: 9296
    // RTCP Transport Feedback NACK BLP: 0xffff (Frames 9297 9298 9299 9300 9301 9302 9303 9304 9305 9306 9307 9308 9309 9310 9311 9312 lost)
    // RTCP Transport Feedback NACK PID: 9313
    // RTCP Transport Feedback NACK BLP: 0xffff (Frames 9314 9315 9316 9317 9318 9319 9320 9321 9322 9323 9324 9325 9326 9327 9328 9329 lost)

    pub fn pairs(&mut self, head_seq_number: u32) -> (Option<Vec<NackPair>>, bool) {
        if self.nacks.is_empty() {
            return (None, false);
        }

        let mut np = NackPair {
            packet_id: 0,
            lost_packets: 0,
        };
        let mut nps: Vec<NackPair> = Vec::new();
        let mut ask_key_frame: bool = false;
        let mut idx = 0_usize;

        while idx < self.nacks.len() {
            let v = &mut self.nacks[idx];

            if v.nackd >= MAX_NACK_TIMES {
                if v.seq_number > self.key_frame_seq_number {
                    self.key_frame_seq_number = v.seq_number;
                    ask_key_frame = true;
                }

                self.nacks.remove(idx);
                continue;
            }

            idx += 1;

            if v.seq_number >= head_seq_number - 2 {
                continue;
            }

            v.nackd += 1;

            if np.packet_id == 0 || v.seq_number as u16 > np.packet_id + 16 {
                if np.packet_id != 0 {
                    // note: `#[warn(clippy::clone_on_copy)]` on by default
                    //help: for further information visit https://rust-lang.github.io/rust-clippy/master/index.html#clone_on_copy
                    nps.push(np);
                }

                np.packet_id = v.seq_number as u16;
                np.lost_packets = 0;

                continue;
            }

            np.lost_packets |= 1 << (v.seq_number as u16 - np.packet_id - 1);
        }

        if np.packet_id != 0 {
            nps.push(np);
        }

        (Some(nps), ask_key_frame)
    }
}