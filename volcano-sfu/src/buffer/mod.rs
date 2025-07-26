pub mod bucket;
pub mod error;
mod factory;
pub use factory::*;
pub mod nack;
pub mod rtcp_reader;

pub use error::BufferError;
use error::Result;

use bucket::Bucket;
use nack::NackQueue;
use webrtc::rtp::extension::transport_cc_extension::TransportCcExtension;

use async_trait::async_trait;
use std::collections::VecDeque;
use std::future::Future;
use std::time::Instant;
use std::{pin::Pin, sync::Arc};
use tokio::sync::Mutex;
use tokio::time::{Duration, sleep};
use webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use webrtc::rtcp::transport_feedbacks::transport_layer_nack::TransportLayerNack;
use webrtc::rtp::extension::audio_level_extension::AudioLevelExtension;
use webrtc::rtp::packet::Packet;
use webrtc::sdp::extmap;
use webrtc::util::Unmarshal;

use rtp::rtp_codec::{RTCRtpParameters, RTPCodecType};
use webrtc::rtcp::packet::Packet as RtcpPacket;
use webrtc::rtp_transceiver as rtp;

const INITIAL_PACKET_PROBE_COUNT: u8 = 25;
const MAX_SEQUENCE_NUMBER: u32 = 1 << 16;
const REPORT_DELTA: f64 = 1e9;

pub type OnCloseFn =
    Box<dyn (FnMut() -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'static>>) + Send + Sync>;
pub type OnTransportWideCCFn = Box<
    dyn (FnMut(u16, u32, bool) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>) + Send + Sync,
>;
pub type OnFeedbackCallBackFn = Box<
    dyn (FnMut(
            Vec<Box<dyn RtcpPacket + Send + Sync>>,
        ) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>)
        + Send
        + Sync,
>;

pub type OnAudioLevelFn =
    Box<dyn (FnMut(bool, u8) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>) + Send + Sync>;
pub enum BufferPacketType {
    RTPBufferPacket = 1,
    RTCPBufferPacket = 2,
}

#[async_trait]
pub trait BufferIO {
    async fn read(&mut self) -> Result<Packet>;
    async fn write(&self, pkt: Packet);
    async fn close(&self) -> Result<()>;
}

#[derive(Debug, Eq, PartialEq, Default, Clone, Copy)]
pub struct VP8 {
    pub temporal_supported: bool,
    // Optional Header
    pub picture_id: u16, /* 8 or 16 bits, picture ID */
    pub picture_id_idx: i32,
    pub mbit: bool,
    pub tl0_picture_idx: u8, /* 8 bits temporal level zero index */
    pub tlz_idx: i32,

    // Optional Header If either of the T or K bits are set to 1,
    // the TID/Y/KEYIDX extension field MUST be present.
    pub tid: u8, /* 2 bits temporal layer idx*/
    // IsKeyFrame is a helper to detect if current packet is a keyframe
    pub is_key_frame: bool,
}

#[derive(Debug, Eq, PartialEq, Default, Clone)]
struct PendingPackets {
    arrival_time: u32,
    pub packet: Packet,
}
#[derive(Debug, Eq, PartialEq, Default, Clone)]
pub struct ExtPacket {
    pub head: bool,
    cycle: u32,
    /// The arrival time of the packet (in nanoseconds).
    pub arrival: u32,
    pub packet: Packet,
    pub key_frame: bool,
    pub payload: VP8,
}

#[derive(Debug, PartialEq, Default, Clone)]
pub struct Stats {
    pub last_expected: u32,
    pub last_received: u32,
    lost_rate: f32,
    pub packet_count: u32, // Number of packets received from this source.
    jitter: f64, // An estimate of the statistical variance of the RTP data packet inter-arrival time.
    pub total_byte: u64,
}

#[derive(Debug, Eq, PartialEq, Default, Clone)]
pub struct Options {
    pub max_bitrate: u64,
}

#[derive(Default, Clone)]
pub struct Buffer {
    bucket: Option<Bucket>,
    nacker: Option<NackQueue>,

    codec_type: RTPCodecType,
    ext_packets: VecDeque<ExtPacket>,
    pending_packets: Vec<PendingPackets>,
    media_ssrc: u32,
    clock_rate: u32,
    max_bitrate: u64,
    /// Time when the last packet was reported (in nanoseconds).
    last_report: u32,
    twcc_ext: u8,
    audio_ext: u8,
    bound: bool,
    closed: bool,
    mime: String,

    // supported feedbacks
    remb: bool,
    nack: bool,
    twcc: bool,
    audio_level: bool,

    min_packet_probe: u8,
    last_packet_read: i32,

    pub max_temporal_layer: i32,
    pub bitrate: u64,
    bitrate_helper: u64,
    last_srntp_time: u64,
    last_srrtp_time: u32,
    last_sr_recv: i64, // Represents wall clock of the most recent sender report arrival
    /// The lowest sequence number received in packet probe.
    base_sn: u16,
    cycles: u32,
    #[allow(dead_code)]
    last_rtcp_packet_time: i64, // Time the last RTCP packet was received.
    #[allow(dead_code)]
    last_rtcp_sr_time: i64, // Time the last RTCP SR was received. Required for DLSR computation.
    /// The latest transit time.
    last_transit: f64,
    /// The highest sequence number received.
    max_seq_no: u16,

    stats: Stats,
    /// The latest timestamp received on a packet.
    /// The timestamp reflects the sampling instant of the first octet in the RTP data packet.
    latest_timestamp: u32,
    /// The latest timestamp received on a packet marked as head.
    /// When there is no head, you should mark as head the first frame incoming every second.
    latest_head_timestamp: u32,
    /// Subsecond timestamp when the latest timestamp was received.
    latest_timestamp_nanos: u32,

    video_pool_len: usize,
    audio_pool_len: usize,
    //callbacks
    //on_close_handler: Arc<Mutex<Option<OnCloseFn>>>,
    on_transport_wide_cc_handler: Arc<Mutex<Option<OnTransportWideCCFn>>>,
    on_feedback_callback_handler: Arc<Mutex<Option<OnFeedbackCallBackFn>>>,
    on_audio_level: Arc<Mutex<Option<OnAudioLevelFn>>>,
}

impl Buffer {
    pub fn new(ssrc: u32) -> Self {
        Self {
            media_ssrc: ssrc,
            video_pool_len: 1500 * 500,
            audio_pool_len: 1500 * 25,
            ..Default::default()
        }
    }

    /// # Responsabilities
    /// - Update SN, max SN and last report
    /// - Add missing sequence numbers to nack queue
    /// - Remove old missing sequence numbers from nack queue
    fn calculate_nack(&mut self, sn: u16, arrival_time: u32) {
        let distance = bucket::distance(sn, self.max_seq_no);
        if self.stats.packet_count == 0 {
            self.base_sn = sn;
            self.max_seq_no = sn;
            self.last_report = arrival_time;
        }
        // new SN
        else if distance & 0x8000 == 0 {
            if sn < self.max_seq_no {
                self.cycles += MAX_SEQUENCE_NUMBER;
            }
            if self.nack {
                // Example: sn=1005, max_seq_no=1000
                // diff = 5 (5 packets lost)
                let diff = sn - self.max_seq_no;

                for i in 1..diff {
                    let msn = sn - i;

                    let ext_sn = self.into_extended_sn(msn);

                    if let Some(nacker) = self.nacker.as_mut() {
                        nacker.push(ext_sn);
                    }
                }
            }
            self.max_seq_no = sn;
        }
        // High distance, sequence number should be removed
        else if self.nack && (distance & 0x8000 > 0) {
            let ext_sn = self.into_extended_sn(sn);
            if let Some(nacker) = self.nacker.as_mut() {
                nacker.remove(ext_sn);
            }
        }
    }

    /// Converts a 16-bit sequence number into 32-bit extended sequence number,
    /// based on actual cycle count and last sequence number got.
    fn into_extended_sn(&self, sn: u16) -> u32 {
        if sn > self.max_seq_no && (sn & 0x8000) > 0 && self.max_seq_no & 0x8000 == 0 {
            // Example: cycles (65536) - 65536 = 0, 0 | sn (32768) = 32768
            (self.cycles - MAX_SEQUENCE_NUMBER) | sn as u32
        } else {
            // Example: cycles (65536) | sn (1001) = 66537
            self.cycles | sn as u32
        }
    }

    /// Build RTCP feedback packets (NACK and PLI) based on detected loss.
    /// 
    /// # Returns 
    /// - RTCP packets vector ready to be sent (may be empty)
    async fn build_feedback_packets(&mut self) -> Vec<Box<dyn RtcpPacket + Send + Sync>> {
        match self.nacker.as_mut() {
            Some(nacker) => {
                let seq_number = self.cycles | self.max_seq_no as u32;
                let (nacks, ask_key_frame) = nacker.pairs(seq_number);

                let mut pkts: Vec<Box<dyn RtcpPacket + Send + Sync>> = Vec::default();
        
                // Add NACKs
                if let Some(nacks) = nacks {
                    if !nacks.is_empty() {
                        let pkt = TransportLayerNack {
                            media_ssrc: self.media_ssrc,
                            nacks: nacks,
                            ..Default::default()
                        };
                        pkts.push(Box::new(pkt));
                    }
                };

                // Add PLI
                if ask_key_frame {
                    let pkt = PictureLossIndication {
                        media_ssrc: self.media_ssrc,
                        ..Default::default()
                    };
                    pkts.push(Box::new(pkt));
                }

                pkts
            },
            None => Vec::default(),
        }
    }

    /// Calculates and updates jitter for the given transit time.
    fn calculate_jitter(&mut self, transit: f64) {
        if self.last_transit != 0.0 {
            let d = (transit - self.last_transit).abs();
            self.stats.jitter += (d - self.stats.jitter) / 16.0;
        }
        self.last_transit = transit;
    }
}

pub struct AtomicBuffer {
    buffer: Arc<Mutex<Buffer>>,
}

#[async_trait]
impl BufferIO for AtomicBuffer {
    /// Adds a RTP Packet, out of order, new packet may be arrived later
    async fn write(&self, pkt: Packet) {
        {
            let mut buffer = self.buffer.lock().await;

            if !buffer.bound {
                buffer.pending_packets.push(PendingPackets {
                    arrival_time: Instant::now().elapsed().subsec_nanos(),
                    packet: pkt.clone(),
                });

                return;
            }
        }

        self.calc(pkt, Instant::now().elapsed().subsec_nanos())
            .await;
    }

    async fn read(&mut self) -> Result<Packet> {
        let buffer = self.buffer.lock().await;
        if buffer.closed {
            return Err(BufferError::ErrIOEof);
        }

        if buffer.pending_packets.len() > buffer.last_packet_read as usize {
            /*
            if buff.len()
                < buffer
                    .pending_packets
                    .get(buffer.last_packet_read as usize)
                    .unwrap()
                    .packet.
            {
                return Err(BufferError::ErrBufferTooSmall);
            }*/

            let packet = &buffer
                .pending_packets
                .get(buffer.last_packet_read as usize)
                .unwrap()
                .packet;

            //n = packet.len();

            //buff.copy_from_slice(&packet[..]);
            return Ok(packet.clone());
        }

        Err(BufferError::ErrNilPacket)
    }

    async fn close(&self) -> Result<()> {
        //let buffer = self.buffer.lock().await;
        //if buffer.bucket.is_some() && buffer.codec_type == RTPCodecType::Video {}

        Ok(())
    }
}

impl AtomicBuffer {
    pub fn new(ssrc: u32) -> Self {
        Self {
            buffer: Arc::new(Mutex::new(Buffer::new(ssrc))),
        }
    }

    pub async fn bind(&self, params: RTCRtpParameters, o: Options) {
        let mut buffer = self.buffer.lock().await;
        let codec = &params.codecs[0];
        buffer.clock_rate = codec.capability.clock_rate;
        buffer.max_bitrate = o.max_bitrate;
        buffer.mime = codec.capability.mime_type.to_lowercase();
        info!("Buffer bind: {}", buffer.mime);

        if buffer.mime.starts_with("audio/") {
            buffer.codec_type = RTPCodecType::Audio;
            buffer.bucket = Some(Bucket::new(buffer.audio_pool_len));
        } else if buffer.mime.starts_with("video/") {
            buffer.codec_type = RTPCodecType::Video;
            buffer.bucket = Some(Bucket::new(buffer.video_pool_len));
        } else {
            buffer.codec_type = RTPCodecType::Unspecified;
        }

        for ext in &params.header_extensions {
            if ext.uri == extmap::TRANSPORT_CC_URI {
                buffer.twcc_ext = ext.id as u8;
                break;
            }
        }

        match buffer.codec_type {
            RTPCodecType::Video => {
                for feedback in &codec.capability.rtcp_feedback {
                    match feedback.typ.clone().as_str() {
                        rtp::TYPE_RTCP_FB_GOOG_REMB => {
                            buffer.remb = true;
                        }
                        rtp::TYPE_RTCP_FB_TRANSPORT_CC => {
                            buffer.twcc = true;
                        }
                        rtp::TYPE_RTCP_FB_NACK => {
                            buffer.nack = true;
                            buffer.nacker = Some(NackQueue::new());
                        }
                        _ => {}
                    }
                }
            }
            RTPCodecType::Audio => {
                for ext in &params.header_extensions {
                    if ext.uri == extmap::AUDIO_LEVEL_URI {
                        buffer.audio_level = true;
                        buffer.audio_ext = ext.id as u8;
                    }
                }
            }

            _ => {}
        }

        debug!(
            "bind -> Processing {} packets",
            buffer.pending_packets.len()
        );
        for pp in buffer.pending_packets.clone() {
            self.calc(pp.packet, pp.arrival_time).await;
        }
        debug!("bind -> Binding done");

        buffer.pending_packets.clear();
        buffer.bound = true;
    }

    pub async fn bitrate(&self) -> u64 {
        self.buffer.lock().await.bitrate
    }

    /// Updates the buffer with the given packet and its arrival time.
    /// The packet is added to the bucket. An [ExtPacket] is created and inserted into the packet queue.
    /// # Responsabilites
    /// - NACK calculation
    /// - Buffer stats
    /// - Store ExtPackets
    /// - Calculate jitter
    /// - Call twcc, audio level, and feedback nack handlers
    /// - Calculate bitrate
    pub async fn calc(&self, packet: Packet, arrival_time: u32) {
        let mut buffer = self.buffer.lock().await;
        let sn = packet.header.sequence_number;
        buffer.calculate_nack(sn, arrival_time);

        let pkt = &packet.payload;
        let max_seq_no = buffer.max_seq_no;
        if let Some(bucket) = buffer.bucket.as_mut() {
            if let Err(err) = bucket.add_packet(pkt, sn, sn == max_seq_no) {
                warn!("Packet #{sn} content not added: {err}");
            }
        }

        // Better safe (wrapping) than sorry (overflow)
        buffer.stats.total_byte = buffer.stats.total_byte.wrapping_add(pkt.len() as u64);
        buffer.bitrate_helper = buffer.bitrate_helper.wrapping_add(pkt.len() as u64);
        buffer.stats.packet_count = buffer.stats.packet_count.wrapping_add(1);

        let mut ep = ExtPacket {
            cycle: buffer.cycles,
            packet: packet.clone(),
            arrival: arrival_time,
            key_frame: false,
            ..Default::default()
        };

        match buffer.codec_type {
            RTPCodecType::Audio => {
                let mut head = packet.header.marker;
                let pkt_ts = packet.header.timestamp;
                // Set initial timestamp for first packet
                if buffer.stats.packet_count == 1 {
                    head = true;
                    buffer.latest_head_timestamp = pkt_ts;
                }
                if buffer.latest_head_timestamp.wrapping_add(buffer.clock_rate) < pkt_ts {
                    buffer.latest_head_timestamp = pkt_ts;
                    head = true;
                }
                ep.head = head;
            }
            _ => {
                // TODO: Head for video codecs (if not implemented)
            }
        }

        match buffer.mime.as_str() {
            "video/vp8" => {
                let mut vp8_packet = VP8::default();
                if let Err(e) = vp8_packet.unmarshal(&packet.payload[..]) {
                    match e {
                        BufferError::ErrNilPacket => {}
                        _ => warn!("Error parsing VP8 packet: {e}"),
                    }
                }
                ep.key_frame = vp8_packet.is_key_frame;
                ep.payload = vp8_packet;
            }
            "video/h264" => {
                ep.key_frame = is_h264_keyframe(&packet.payload[..]);
            }
            mime => {
                if mime.starts_with("video/") {
                    debug!("Unsupported MIME type: {mime}. Ignored.");
                }
            }
        }

        if buffer.min_packet_probe < INITIAL_PACKET_PROBE_COUNT {
            if sn < buffer.base_sn {
                buffer.base_sn = sn
            }

            if buffer.mime == "video/vp8" {
                let pld = ep.payload;
                let mtl = buffer.max_temporal_layer;
                if mtl < pld.tid as i32 {
                    buffer.max_temporal_layer = pld.tid as i32;
                }
            }

            buffer.min_packet_probe += 1;
        }

        buffer.ext_packets.push_back(ep);

        // if first time update or the timestamp is later (factoring timestamp wrap around)
        let latest_timestamp = buffer.latest_timestamp;
        let subsec_nanos = buffer.latest_timestamp_nanos;
        if (subsec_nanos == 0) || is_later_timestamp(packet.header.timestamp, latest_timestamp) {
            buffer.latest_timestamp = packet.header.timestamp;
            buffer.latest_timestamp_nanos = arrival_time;
        }

        let arrival = arrival_time as f64 / 1e6 * (buffer.clock_rate as f64 / 1e3);
        let transit = arrival - packet.header.timestamp as f64;
        buffer.calculate_jitter(transit);

        if buffer.twcc {
            if let Some(ext) = packet.header.get_extension(buffer.twcc_ext) {
                match TransportCcExtension::unmarshal(&mut &ext[..]) {
                    Ok(data) => {
                        let mut handler = buffer.on_transport_wide_cc_handler.lock().await;
                        if let Some(f) = &mut *handler {
                            f(data.transport_sequence, arrival_time, packet.header.marker).await;
                        }
                    }
                    Err(err) => {
                        error!("Error parsing transport wide cc extension: {err}");
                    }
                };
            }
        }

        if buffer.audio_level {
            if let Some(ext) = packet.header.get_extension(buffer.audio_ext) {
                let rv = AudioLevelExtension::unmarshal(&mut &ext[..]);

                if let Ok(data) = rv {
                    let mut handler = buffer.on_audio_level.lock().await;
                    if let Some(f) = &mut *handler {
                        f(data.voice, data.level).await;
                    }
                }
            }
        }

        let diff = arrival_time.saturating_sub(buffer.last_report);

        if buffer.nacker.is_some() {
            let rv = buffer.build_feedback_packets().await;
            let mut handler = buffer.on_feedback_callback_handler.lock().await;
            if let Some(f) = &mut *handler {
                f(rv).await;
            }
        }

        if diff >= REPORT_DELTA as u32 {
            if diff > 0 {
                let br = 8 * buffer.bitrate_helper * REPORT_DELTA as u64 / diff as u64;
                buffer.bitrate = br;
            } else {
                warn!("Attemped to divide by zero. Skipped bitrate.");
            }
            buffer.last_report = arrival_time;
            buffer.bitrate_helper = 0;
        }
    }

    pub async fn get_packet(&self, buff: &mut [u8], sn: u16) -> Result<usize> {
        let buffer = self.buffer.lock().await;

        if buffer.closed {
            return Err(BufferError::ErrIOEof);
        }

        if let Some(bucket) = &buffer.bucket {
            return bucket.get_packet(buff, sn);
        }

        Ok(0)
    }

    pub async fn get_sender_report_data(&self) -> (u32, u64, i64) {
        let buffer = self.buffer.lock().await;
        (
            buffer.last_srrtp_time,
            buffer.last_srntp_time,
            buffer.last_sr_recv,
        )
    }

    pub async fn get_status(&self) -> Stats {
        self.buffer.lock().await.stats.clone()
    }

    pub async fn max_temporal_layer(&self) -> i32 {
        self.buffer.lock().await.max_temporal_layer
    }

    /// Loops until find the first ext packet in buffer and returns it.
    pub async fn read_extended(&self) -> Result<ExtPacket> {
        loop {
            if self.buffer.lock().await.closed {
                return Err(BufferError::ErrIOEof);
            }

            let ext_packets = &mut self.buffer.lock().await.ext_packets;
            if !ext_packets.is_empty() {
                if let Some(pkt) = ext_packets.pop_front() {
                    return Ok(pkt);
                };
            }
            sleep(Duration::from_millis(10)).await;
        }
    }

    pub async fn register_on_audio_level(&self, f: OnAudioLevelFn) {
        let buffer = self.buffer.lock().await;
        let mut handler = buffer.on_audio_level.lock().await;
        *handler = Some(f);
    }

    pub async fn register_on_feedback(&self, f: OnFeedbackCallBackFn) {
        let buffer = self.buffer.lock().await;
        let mut handler = buffer.on_feedback_callback_handler.lock().await;
        *handler = Some(f);
    }

    pub async fn set_sender_report_data(&self, rtp_time: u32, ntp_time: u64) {
        let mut buffer = self.buffer.lock().await;

        buffer.last_srrtp_time = rtp_time;
        buffer.last_srntp_time = ntp_time;
        buffer.last_sr_recv = Instant::now().elapsed().subsec_nanos() as i64;
    }
}

impl VP8 {
    pub fn unmarshal(&mut self, payload: &[u8]) -> Result<()> {
        let payload_len = payload.len();
        if payload_len == 0 {
            return Err(BufferError::ErrNilPacket);
        }

        let mut idx: usize = 0;
        let s = payload[idx] & 0x10 > 0;
        if payload[idx] & 0x80 > 0 {
            idx += 1;

            if payload_len < idx + 1 {
                return Err(BufferError::ErrShortPacket);
            }

            self.temporal_supported = payload[idx] & 0x20 > 0;
            let k = payload[idx] & 0x10 > 0;
            let l = payload[idx] & 0x40 > 0;

            // Check for PictureID
            if payload[idx] & 0x80 > 0 {
                idx += 1;
                if payload_len < idx + 1 {
                    return Err(BufferError::ErrShortPacket);
                }
                self.picture_id_idx = idx as i32;
                let pid = payload[idx] & 0x7f;
                // Check if m is 1, then Picture ID is 15 bits
                if payload[idx] & 0x80 > 0 {
                    idx += 1;
                    if payload_len < idx + 1 {
                        return Err(BufferError::ErrShortPacket);
                    }
                    self.mbit = true;

                    self.picture_id = ((pid as u16) << 8) | payload[idx] as u16;
                } else {
                    self.picture_id = pid as u16;
                }
            }

            // Check if TL0PICIDX is present
            if l {
                idx += 1;
                if payload_len < idx + 1 {
                    return Err(BufferError::ErrShortPacket);
                }
                self.tlz_idx = idx as i32;

                if idx >= payload_len {
                    return Err(BufferError::ErrShortPacket);
                }
                self.tl0_picture_idx = payload[idx];
            }

            if self.temporal_supported || k {
                idx += 1;
                if payload_len < idx + 1 {
                    return Err(BufferError::ErrShortPacket);
                }
                self.tid = (payload[idx] & 0xc0) >> 6;
            }

            if idx >= payload_len {
                return Err(BufferError::ErrShortPacket);
            }
            idx += 1;
            if payload_len < idx + 1 {
                return Err(BufferError::ErrShortPacket);
            }
            // Check is packet is a keyframe by looking at P bit in vp8 payload
            self.is_key_frame = payload[idx] & 0x01 == 0 && s;
        } else {
            idx += 1;
            if payload_len < idx + 1 {
                return Err(BufferError::ErrShortPacket);
            }
            // Check is packet is a keyframe by looking at P bit in vp8 payload
            self.is_key_frame = payload[idx] & 0x01 == 0 && s;
        }

        Ok(())
    }
}

fn is_h264_keyframe(payload: &[u8]) -> bool {
    if payload.is_empty() {
        return false;
    }
    let nalu = payload[0] & 0x1F;
    if nalu == 0 {
        // reserved
        return false;
    } else if nalu <= 23 {
        // simple NALU
        return nalu == 5;
    } else if nalu == 24 || nalu == 25 || nalu == 26 || nalu == 27 {
        // STAP-A, STAP-B, MTAP16 or MTAP24
        let mut i = 1;
        if nalu == 25 || nalu == 26 || nalu == 27 {
            // skip DON
            i += 2;
        }
        while i < payload.len() {
            if i + 2 > payload.len() {
                return false;
            }
            let length = ((payload[i] as u16) << 8) | payload[i + 1] as u16;
            i += 2;
            if i + length as usize > payload.len() {
                return false;
            }
            let mut offset = 0;
            if nalu == 26 {
                offset = 3;
            } else if nalu == 27 {
                offset = 4;
            }
            if offset >= length {
                return false;
            }
            let n = payload[i + offset as usize] & 0x1F;
            if n == 7 {
                return true;
            } else if n >= 24 {
                // is this legal?
            }
            i += length as usize;
        }
        if i == payload.len() {
            return false;
        }
        return false;
    } else if nalu == 28 || nalu == 29 {
        // FU-A or FU-B
        if payload.len() < 2 {
            return false;
        }
        if (payload[1] & 0x80) == 0 {
            // not a starting fragment
            return false;
        }
        return payload[1] & 0x1F == 7;
    }
    false
}

// is_timestamp_wrap_around returns true if wrap around happens from timestamp1 to timestamp2
pub fn is_timestamp_wrap_around(timestamp1: u32, timestamp2: u32) -> bool {
    (timestamp1 & 0xC000000 == 0) && (timestamp2 & 0xC000000 == 0xC000000)
}

// is_later_timestamp returns true if timestamp1 is later in time than timestamp2 factoring in timestamp wrap-around
fn is_later_timestamp(timestamp1: u32, timestamp2: u32) -> bool {
    if timestamp1 > timestamp2 {
        if is_timestamp_wrap_around(timestamp2, timestamp1) {
            return false;
        }
        return true;
    }
    if is_timestamp_wrap_around(timestamp1, timestamp2) {
        return true;
    }
    false
}
