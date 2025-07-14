use byteorder::{BigEndian, ByteOrder};

use super::error::{BufferError, Result};

/// Maximum packet size in bytes. including the header and the packet size.
const MAX_PACKET_SIZE: usize = 1500;

/// Calculates the distance between two RTP sequence numbers (16-bit),
/// taking into account their **circular** behavior from `0` to `65535`.
///
/// RTP sequence numbers are 16-bit unsigned integers that wrap around:
///
///     ... 65533, 65534, 65535, 0, 1, 2 ...
///
/// This function computes how many steps ahead `new` is from `old` in the circular sequence.
pub fn distance(new: u16, old: u16) -> u16 {
    ((new as i16) - (old as i16)) as u16
}

#[derive(Debug, Eq, PartialEq, Default, Clone)]
/// A circular buffer that stores packets in the order of their sequence numbers.
/// For lost packets, [MAX_PACKET_SIZE] bytes are reserved in the buffer.
/// 
/// This bucket requires a minimum size of [MAX_PACKET_SIZE] bytes.
/// 
/// # Packet format
/// ```txt
/// 0                   1                   2                   3
/// 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |SZ |V=2|P|X|  CC   |M|     PT    |       sequence number     |
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |                              ...                            |
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// ```
/// Where SZ is the size of the packet injected before the RTP header.
pub struct Bucket {
    buf: Vec<u8>,
    init: bool,
    step: usize,
    head_sn: u16,
    max_steps: usize,
}

impl Bucket {

    /// Creates a new bucket with the given capacity.
    /// 
    /// # Arguments
    /// - `length`: The capacity of the bucket. This value cannot be less than [MAX_PACKET_SIZE].
    pub fn new(length: usize) -> Self {
        assert!(length >= MAX_PACKET_SIZE, "Minimum bucket size must be {}", MAX_PACKET_SIZE);
        Self {
            buf: vec![0; length],
            init: false,
            step: 0,
            head_sn: 0,
            max_steps: (length / MAX_PACKET_SIZE).saturating_sub(1),
        }
    }

    pub fn add_packet(&mut self, pkt: &[u8], sn: u16, latest: bool) -> Result<Vec<u8>> {
        if !self.init {
            self.head_sn = sn.wrapping_sub(1);
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

        // max_steps = (capacity / MAX_PACKET_SIZE) - 1
        // If step ~= max_steps, then pkt_len_idx ~= capacity - MAX_PACKET_SIZE
        let pkt_len_idx = self.step * MAX_PACKET_SIZE;

        // Prevent memory overflow
        if self.buf.capacity() < pkt_len_idx + pkt_len {
            return Err(BufferError::ErrBufferTooSmall);
        }

        // Prevent overlapping
        if pkt_len + 2 > MAX_PACKET_SIZE {
            return Err(BufferError::ErrLargePacket);
        }

        // Write operation
        BigEndian::write_u16(&mut self.buf[pkt_len_idx..], pkt_len as u16);

        let off = pkt_len_idx + 2;

        self.buf[off..off + pkt_len].copy_from_slice(pkt);

        // step < usize::MAX. Increment is safe.
        self.step += 1;

        if self.step > self.max_steps {
            // Reset step
            self.step = 0;
        }

        Ok(self.buf[off..off + pkt_len].to_vec())
    }

    /// Returns a copy of the packet looked up by its sequence number, or [None] if it is not found.
    pub fn get(&self, sn: u16) -> Option<Vec<u8>> {
        let diff: u16 = distance(self.head_sn, sn);

       // max_steps + 1 < usize::MAX. Add is safe.
       let max_step = self.max_steps + 1;
       let pos = (self.step + max_step - (diff as usize + 1)) % max_step;
        
        let off = pos * MAX_PACKET_SIZE;
        let capacity = self.buf.len();

        if off > capacity {
            warn!("Unreachable packet with SN {sn}, distance={diff}");
            return None;
        }

        let actual_sn = Self::get_packet_sn(&self.buf, off);
        if actual_sn != sn {
            warn!("Mismatched packet with SN {actual_sn}, expected {sn}");
            return None;
        }

        let size = BigEndian::read_u16(&self.buf[off..]);

        let start = off + 2;
        let end = start.saturating_add(size as usize); // Prevent overflow

        if end > capacity {
            warn!("Tried to read packet with size {size}, but {0} > capacity ({capacity})", end);
            return None;
        }

        Some(self.buf[start..end].to_vec())
    }

    fn set(&mut self, sn: u16, pkt: &[u8]) -> Result<Vec<u8>> {
        let diff: u16 = distance(self.head_sn, sn);
        if diff > self.max_steps as u16 {
            return Err(BufferError::ErrPacketTooOld);
        }

        // max_steps + 1 < usize::MAX. Add is safe.
        let max_step = self.max_steps + 1;
        let pos = (self.step + max_step - (diff as usize + 1)) % max_step;

        // If pos < max_step, then (pos * MAX_PACKET_SIZE) < capacity
        // off < capacity =< usize::MAX. Multiplication is safe.
        let off = pos * MAX_PACKET_SIZE;
        if off > self.buf.len() {
            return Err(BufferError::ErrPacketTooOld);
        }

        if Self::get_packet_sn(&self.buf, off) == sn {
            return Err(BufferError::ErrRTXPacket);
        }

        let pkt_len = pkt.len();

        if pkt_len + 2 > MAX_PACKET_SIZE {
            return Err(BufferError::ErrLargePacket);
        }

        // Write operation
        BigEndian::write_u16(&mut self.buf[off..], pkt_len as u16);

        self.buf[off + 2..off + 2 + pkt_len].copy_from_slice(pkt);

        Ok(pkt.to_vec())
    }

    fn get_packet_sn(pkt: &[u8], offset: usize) -> u16 {
        let padding = offset + 4;
        BigEndian::read_u16(&pkt[padding..])
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_distance() {
        // 65535 <- 0
        assert_eq!(distance(0, 65535), 1);
        // 65534 <- 65535 <- 0 <- 1
        assert_eq!(distance(1, 65534), 3);
        assert_eq!(distance(100, 90), 10);
        assert_eq!(distance(65535, 0), 65535);
    }

    #[test]
    fn test_bucket_low_capacity() {
        let mut bucket = Bucket::new(MAX_PACKET_SIZE);
        let pkt = vec![0x01; MAX_PACKET_SIZE];
        assert!(bucket.add_packet(&pkt, 1, true).is_ok(), "Bucket should have enough capacity");
        //assert_eq!(bucket.add_packet(&pkt, 1, true), Err(BufferError::ErrBufferTooSmall));
    }

    #[test]
    fn test_add_packets() {
        // This bucket has a capacity for 3 packets
        let mut bucket = Bucket::new(MAX_PACKET_SIZE * 3);
        assert!(!bucket.init, "Bucket should not be initialized");
        assert_eq!(bucket.max_steps, 2, "Bucket step should not be higher than 2");

        // First packet is... the first one
        let pkt = vec![0x01; 500];
        let res1 = bucket.add_packet(&pkt, 100, true);
        assert!(res1.is_ok());

        assert_eq!(bucket.head_sn, 100, "100 should be the last SN");
        assert!(bucket.init, "Bucket should be initialized");
        
        // Second packet is a new one
        let pkt2 = vec![0x02; 500];
        let res2 = bucket.add_packet(&pkt2, 102, true);
        assert!(res2.is_ok());

        assert_eq!(bucket.head_sn, 102, "102 should be the last SN");
        assert_eq!(bucket.step, 1, "Step should be 1");

        // Third packet is late
        let pkt3 = vec![0x03; 1_000];
        let res3 = bucket.add_packet(&pkt3, 101, false);
        assert!(res3.is_ok());
        assert_eq!(bucket.head_sn, 102, "102 should be the last SN");

        assert_eq!(bucket.step, 2, "Step should be 2");
    }

    #[test]
    fn test_add_wraparound() {
        let mut bucket = Bucket::new(1_000);
        bucket.init = true;
        bucket.head_sn = 65535;

        let pkt = vec![0x01; 500];

        let res = bucket.add_packet(&pkt, 0, true);
        assert!(res.is_ok());

        assert_eq!(bucket.head_sn, 0, "Head should be 0");
        assert_eq!(bucket.step, 0, "Step 0");
    }
}