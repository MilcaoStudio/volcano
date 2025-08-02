pub mod bucket;
pub mod error;
mod factory;
pub use factory::*;
pub mod nack;
pub mod rtcp;

pub use error::BufferError;
use error::Result;

use bucket::Bucket;
use nack::NackQueue;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::time::Instant;
use webrtc::rtp::extension::transport_cc_extension::TransportCcExtension;

use async_trait::async_trait;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::{Duration};
use std::{pin::Pin, sync::Arc};
use tokio::sync::{Mutex, broadcast};
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
const REPORT_DELTA: u128 = 1_000_000_000;

pub type OnCloseFn =
    Box<dyn (FnMut() -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'static>>) + Send + Sync>;
pub type OnTransportWideCCFn = Box<
    dyn (FnMut(u16, Duration, bool) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>) + Send + Sync,
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

#[derive(Debug, Eq, PartialEq, Clone)]
struct PendingPackets {
    arrival_instant: Instant,
    pub packet: Packet,
}
#[derive(Debug, Eq, PartialEq, Default, Clone)]
pub struct ExtPacket {
    pub head: bool,
    cycle: u32,
    /// Relative arrival time since a reference instant (e.g. [AtomicBuffer::start_time])
    pub arrival: Duration,
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

pub struct Buffer {
    bucket: Option<Bucket>,
    nacker: Option<NackQueue>,

    codec_type: RTPCodecType,
    media_ssrc: u32,
    clock_rate: u32,
    max_bitrate: u64,
    /// Instant when the last packet was reported.
    last_report: Option<Instant>,
    closed: bool,
    mime: String,

    // supported feedbacks
    remb: bool,
    nack: bool,
    min_packet_probe: u8,
    pub max_temporal_layer: i32,
    pub bitrate: u64,
    bitrate_helper: u64,
    /// Last sender report NTP timestamp
    last_srntp_time: u64,
    /// Last sender report RTP timestamp
    last_srrtp_time: u32,
    /// Subsecond nanos when the last sender report arrived
    last_sr_recv: i64,
    /// The lowest sequence number received in packet probe.
    base_sn: u16,
    cycles: u32,
    #[allow(dead_code)]
    last_rtcp_packet_time: i64, // Time the last RTCP packet was received.
    #[allow(dead_code)]
    last_rtcp_sr_time: i64, // Time the last RTCP SR was received. Required for DLSR computation.
    /// The latest transit time (ticks).
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
    /// Instant when last packet has arrived.
    last_arrival: Option<Instant>,

    video_pool_len: usize,
    audio_pool_len: usize,
}

impl Buffer {
    pub fn new(ssrc: u32) -> Self {
        Self {
            media_ssrc: ssrc,
            video_pool_len: 1500 * 500,
            audio_pool_len: 1500 * 25,
            bucket: Default::default(),
            nacker: Default::default(),
            codec_type: Default::default(),
            //pending_packets: Default::default(),
            clock_rate: Default::default(),
            max_bitrate: Default::default(),
            last_report: Default::default(),
            //twcc_ext: Default::default(),
            //audio_ext: Default::default(),
            //bound: Default::default(),
            closed: Default::default(),
            mime: Default::default(),
            remb: Default::default(),
            nack: Default::default(),
            //twcc: Default::default(),
            //audio_level: Default::default(),
            min_packet_probe: Default::default(),
            //last_packet_read: Default::default(),
            max_temporal_layer: Default::default(),
            bitrate: Default::default(),
            bitrate_helper: Default::default(),
            last_srntp_time: Default::default(),
            last_srrtp_time: Default::default(),
            last_sr_recv: Default::default(),
            base_sn: Default::default(),
            cycles: Default::default(),
            last_rtcp_packet_time: Default::default(),
            last_rtcp_sr_time: Default::default(),
            last_transit: Default::default(),
            max_seq_no: Default::default(),
            stats: Default::default(),
            latest_timestamp: Default::default(),
            latest_head_timestamp: Default::default(),
            last_arrival: Default::default(),
        }
    }

    /// Updates this buffer with the given packet and its arrival time.
    /// The packet is added to the bucket and returned as [ExtPacket].
    /// # Responsabilites
    /// - NACK calculation
    /// - Buffer stats
    /// - Store ExtPackets
    /// - Calculate jitter
    /// - Call twcc, audio level, and feedback nack handlers
    /// - Calculate bitrate
    pub fn process_rtp_packet(&mut self, packet: Packet, now: Instant, arrival_time: Duration) -> ExtPacket {
        let sn = packet.header.sequence_number;
        self.calculate_nack(sn, now);

        let pkt = &packet.payload;
        let max_seq_no = self.max_seq_no;
        if let Some(bucket) = self.bucket.as_mut() {
            if let Err(err) = bucket.add_packet(pkt, sn, sn == max_seq_no) {
                warn!("Packet #{sn} content not added: {err}");
            }
        }

        // Better safe (wrapping) than sorry (overflow)
        self.stats.total_byte = self.stats.total_byte.wrapping_add(pkt.len() as u64);
        self.bitrate_helper = self.bitrate_helper.wrapping_add(pkt.len() as u64);
        self.stats.packet_count = self.stats.packet_count.wrapping_add(1);

        self.last_arrival = Some(now);
        
        let mut ep = ExtPacket {
            cycle: self.cycles,
            packet: packet.clone(),
            arrival: arrival_time,
            key_frame: false,
            ..Default::default()
        };

        match self.codec_type {
            RTPCodecType::Audio => {
                let mut head = packet.header.marker;
                let pkt_ts = packet.header.timestamp;
                // Set initial timestamp for first packet
                if self.stats.packet_count == 1 {
                    head = true;
                    self.latest_head_timestamp = pkt_ts;
                }
                if self.latest_head_timestamp.wrapping_add(self.clock_rate) < pkt_ts {
                    self.latest_head_timestamp = pkt_ts;
                    head = true;
                }
                ep.head = head;
            }
            _ => {
                // TODO: Head for video codecs (if not implemented)
            }
        }

        match self.mime.as_str() {
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

        if self.min_packet_probe < INITIAL_PACKET_PROBE_COUNT {
            if sn < self.base_sn {
                self.base_sn = sn
            }

            if self.mime == "video/vp8" {
                let pld = ep.payload;
                let mtl = self.max_temporal_layer;
                if mtl < pld.tid as i32 {
                    self.max_temporal_layer = pld.tid as i32;
                }
            }

            self.min_packet_probe += 1;
        }

        self.update_timestamp(packet.header.timestamp, now);

        // Ticks measure from arrival time until jitter calculation
        let delta = arrival_time.as_secs_f64() * self.clock_rate as f64;
        let transit = delta - packet.header.timestamp as f64;
        self.calculate_jitter(transit);

        let delta = match self.last_report {
            Some(last) => last.duration_since(now).as_nanos(),
            _ => 0,
        };

        if delta >= REPORT_DELTA {
            let br = 8 * self.bitrate_helper * REPORT_DELTA as u64 / delta as u64;
            self.bitrate = br;
            self.last_report = Some(Instant::now());
            self.bitrate_helper = 0;
        }
        
        if self.last_report.is_none() {
            self.last_report = Some(Instant::now());
        } 

        ep
    }

    /// # Responsabilities
    /// - Update max SN
    /// - Set base SN and last report instant if the first packet arrives
    /// - Add missing sequence numbers to nack queue
    /// - Remove old missing sequence numbers from nack queue
    fn calculate_nack(&mut self, sn: u16, now: Instant) {
        let distance = bucket::distance(sn, self.max_seq_no);
        if self.stats.packet_count == 0 {
            self.base_sn = sn;
            self.max_seq_no = sn;
            self.last_report = Some(now);
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
    fn build_feedback_packets(&mut self) -> Vec<Box<dyn RtcpPacket + Send + Sync>> {
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
            }
            None => Vec::default(),
        }
    }

    /// Calculates and updates jitter for the given transit time (in ticks).
    fn calculate_jitter(&mut self, transit: f64) {
        if self.last_transit != 0.0 {
            let d = (transit - self.last_transit).abs();
            self.stats.jitter += (d - self.stats.jitter) / 16.0;
        }
        self.last_transit = transit;
    }

    /// Updates latest timestamp based on reported timestamp and time in nanoseconds when the packet arrived.
    fn update_timestamp(&mut self, rtp_timestamp: u32, arrival_instant: Instant) {
        // if first time update or the timestamp is later (factoring timestamp wrap around)
        let latest_timestamp = self.latest_timestamp;
        if is_later_timestamp(rtp_timestamp, latest_timestamp) {
            self.latest_timestamp = rtp_timestamp;
            self.last_arrival = Some(arrival_instant);
        }
    }
}

pub struct AtomicBuffer {
    audio_ext: AtomicU8,
    audio_level: AtomicBool,
    bound: AtomicBool,
    buffer: Arc<Mutex<Buffer>>,
    close_sender: broadcast::Sender<()>,
    pub close_rx: Arc<Mutex<broadcast::Receiver<()>>>,
    on_transport_wide_cc_handler: Arc<Mutex<Option<OnTransportWideCCFn>>>,
    on_feedback_callback_handler: Arc<Mutex<Option<OnFeedbackCallBackFn>>>,
    on_audio_level: Arc<Mutex<Option<OnAudioLevelFn>>>,
    pending_packets: Mutex<Vec<PendingPackets>>,
    packet_tx: UnboundedSender<ExtPacket>,
    pub packet_rx: Arc<Mutex<UnboundedReceiver<ExtPacket>>>,
    twcc: AtomicBool,
    twcc_ext: AtomicU8,
    pub start_time: Instant,
}

#[async_trait]
impl BufferIO for AtomicBuffer {
    /// Adds a RTP Packet, out of order, new packet may be arrived later
    async fn write(&self, pkt: Packet) {
        let arrival_instant = Instant::now();
        if !self.bound.load(Ordering::Acquire) {
            let mut pending = self.pending_packets.lock().await;
            pending.push(PendingPackets {
                arrival_instant,
                packet: pkt,
            });

            return;
        }

        let arrival_time = arrival_instant.duration_since(self.start_time);
        {
            let mut buffer = self.buffer.lock().await;
            let ext_packet = buffer.process_rtp_packet(pkt.clone(), arrival_instant, arrival_time);
            if let Err(err) = self.packet_tx.send(ext_packet) {
                warn!("write -> Send packet failed: {err}");
            };

            if buffer.nacker.is_some() {
                let fb_packets = buffer.build_feedback_packets();
                let fb_handler = self.on_feedback_callback_handler.clone();
                tokio::spawn(async move {
                    let mut handler = fb_handler.lock().await;
                    if let Some(f) = &mut *handler {
                        f(fb_packets).await;
                    }
                });
            }

        }

        if self.twcc.load(Ordering::Acquire) {
            if let Some(ext) = pkt
                .header
                .get_extension(self.twcc_ext.load(Ordering::Acquire))
            {
                match TransportCcExtension::unmarshal(&mut &ext[..]) {
                    Ok(data) => {
                        let twcc_handler = self.on_transport_wide_cc_handler.clone();
                        tokio::spawn(async move {
                            let mut handler = twcc_handler.lock().await;
                            if let Some(f) = handler.as_mut() {
                                f(data.transport_sequence, arrival_time, pkt.header.marker).await;
                            }
                        });
                    }
                    Err(err) => {
                        error!("Error parsing transport wide cc extension: {err}");
                    }
                };
            }
        }

        if self.audio_level.load(Ordering::Acquire) {
            if let Some(ext) = pkt
                .header
                .get_extension(self.audio_ext.load(Ordering::Acquire))
            {
                if let Ok(data) = AudioLevelExtension::unmarshal(&mut &ext[..]) {
                    let audio_level_handler = self.on_audio_level.clone();
                    tokio::spawn(async move {
                        let mut handler = audio_level_handler.lock().await;
                        if let Some(f) = handler.as_mut() {
                            f(data.voice, data.level).await;
                        }
                    });
                }
            }
        }
    }

    #[warn(unused)]
    async fn read(&mut self) -> Result<Packet> {
        Err(BufferError::ErrPacketNotFound)
    }

    async fn close(&self) -> Result<()> {
        //let buffer = self.buffer.lock().await;
        //if buffer.bucket.is_some() && buffer.codec_type == RTPCodecType::Video {}
        if let Err(_) = self.close_sender.send(()) {
            warn!("close_rx dropped");
        };

        Ok(())
    }
}

impl AtomicBuffer {
    pub fn new(ssrc: u32) -> Self {
        let (s, r) = broadcast::channel::<()>(1);
        let (pkt_s, pkt_r) = unbounded_channel::<ExtPacket>();
        Self {
            audio_ext: Default::default(),
            audio_level: Default::default(),
            bound: Default::default(),
            buffer: Arc::new(Mutex::new(Buffer::new(ssrc))),
            pending_packets: Default::default(),
            close_sender: s,
            close_rx: Arc::new(Mutex::new(r)),
            start_time: Instant::now(),
            packet_tx: pkt_s,
            packet_rx: Arc::new(Mutex::new(pkt_r)),
            on_audio_level: Default::default(),
            on_feedback_callback_handler: Default::default(),
            on_transport_wide_cc_handler: Default::default(),
            twcc: Default::default(),
            twcc_ext: Default::default(),
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
                self.twcc_ext.store(ext.id as u8, Ordering::Release);
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
                            self.twcc.store(true, Ordering::Release);
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
                        self.audio_level.store(true, Ordering::Release);
                        self.audio_ext.store(ext.id as u8, Ordering::Release);
                    }
                }
            }

            _ => {}
        }

        let mut pending = self.pending_packets.lock().await;

        debug!("bind -> Processing {} packets", pending.len());

        for pp in pending.drain(..) {
            let arrival_time = pp.arrival_instant.duration_since(self.start_time);
            let ext_packet = buffer.process_rtp_packet(pp.packet, pp.arrival_instant, arrival_time);
            if let Err(err) = self.packet_tx.send(ext_packet) {
                warn!("bind -> Send packet failed: {err}")
            };
        }
        self.bound.store(true, Ordering::Release);
    }

    pub async fn bitrate(&self) -> u64 {
        self.buffer.lock().await.bitrate
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

    pub async fn register_on_audio_level(&self, f: OnAudioLevelFn) {
        let mut handler = self.on_audio_level.lock().await;
        *handler = Some(f);
    }

    pub async fn register_on_feedback(&self, f: OnFeedbackCallBackFn) {
        let mut handler = self.on_feedback_callback_handler.lock().await;
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
