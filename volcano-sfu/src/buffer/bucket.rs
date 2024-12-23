use byteorder::{BigEndian, ByteOrder};

use super::error::{BufferError, Result};

const MAX_PACKET_SIZE: usize = 1500;

//For example: sequence numbers inserted are 65533, 65534, the new coming one is 2,
//the new is 2 and old is 65534, the distance between 2 and 65534 is 4 which is
//65535 - 65534 + 2 + 1.(65533,65534,65535,0,1,2)
pub fn distance(new: u16, old: u16) -> u16 {
    if new < old {
        65535 - old + new + 1
    } else {
        new - old
    }
}
#[derive(Debug, Eq, PartialEq, Default, Clone)]
pub struct Bucket {
    buf: Vec<u8>,
    // src: Vec<u8>,
    init: bool,
    step: i32,
    head_sn: u16,
    max_steps: i32,
}

impl Bucket {
    pub fn new(length: usize) -> Self {
        Self {
            buf: vec![0; length],
            // src: Vec::new(),
            init: false,
            step: 0,
            head_sn: 0,
            max_steps: (length / MAX_PACKET_SIZE) as i32 - 1,
        }
    }

    pub fn add_packet(&mut self, pkt: &[u8], sn: u16, latest: bool) -> Result<Vec<u8>> {
        if !self.init {
            self.head_sn = sn - 1;
            self.init = true;
        }

        if !latest {
            return self.set(sn, pkt);
        }

        let diff: u16 = distance(sn, self.head_sn);
        self.head_sn = sn;

        for _ in 1..diff {
            self.step += 1;
            if self.step >= self.max_steps {
                self.step = 0;
            }
        }

        self.push(pkt)
    }

    pub fn get_packet(&self, buf: &mut [u8], sn: u16) -> Result<usize> {
        let p = self.get(sn);

        if p.is_none() {
            return Err(BufferError::ErrPacketNotFound);
        }

        let i = p.clone().unwrap().len();

        if buf.len() < i {
            return Err(BufferError::ErrBufferTooSmall);
        }

        if let Some(data) = p {
            buf.copy_from_slice(&data[..]);
        }

        Ok(i)
    }

    fn push(&mut self, pkt: &[u8]) -> Result<Vec<u8>> {
        let pkt_len = pkt.len();
        let pkt_len_idx = self.step as usize * MAX_PACKET_SIZE;
        if self.buf.capacity() < pkt_len_idx + pkt_len {
            return Err(BufferError::ErrBufferTooSmall);
        }
        BigEndian::write_u16(&mut self.buf[pkt_len_idx..], pkt_len as u16);

        let off = pkt_len_idx + 2;

        self.buf[off..off + pkt_len].copy_from_slice(pkt);

        self.step += 1;

        if self.step > self.max_steps {
            self.step = 0;
        }

        Ok(self.buf[off..off + pkt_len].to_vec())
    }

    pub fn get(&self, sn: u16) -> Option<Vec<u8>> {
        let diff: u16 = distance(self.head_sn, sn);

        let mut pos = self.step - (diff + 1) as i32;
        if pos < 0 {
            if -pos > self.max_steps + 1 {
                return None;
            }
            pos = self.max_steps + pos + 1;
        }

        let off = pos as usize * MAX_PACKET_SIZE;

        if off > self.buf.len() {
            return None;
        }

        if BigEndian::read_u16(&self.buf[off as usize + 4..]) != sn {
            return None;
        }

        let size = BigEndian::read_u16(&self.buf[off as usize..]);

        Some(self.buf[off + 2..off + 2 + size as usize].to_vec())
    }

    fn set(&mut self, sn: u16, pkt: &[u8]) -> Result<Vec<u8>> {
        let diff: u16 = distance(self.head_sn, sn);
        if diff > self.max_steps as u16 {
            return Err(BufferError::ErrPacketTooOld);
        }
        let mut pos = self.step - (diff + 1) as i32;
        if pos < 0 {
            pos = self.max_steps + pos + 1
        }

        let off = pos * MAX_PACKET_SIZE as i32;
        if off > self.buf.len() as i32 || off < 0 {
            return Err(BufferError::ErrPacketTooOld);
        }

        if BigEndian::read_u16(&self.buf[off as usize + 4..]) == sn {
            return Err(BufferError::ErrRTXPacket);
        }

        let pkt_len = pkt.len();

        BigEndian::write_u16(&mut self.buf[off as usize..], pkt_len as u16);

        self.buf[off as usize + 2..off as usize + 2 + pkt_len].copy_from_slice(pkt);

        Ok(pkt.to_vec())
    }
}
