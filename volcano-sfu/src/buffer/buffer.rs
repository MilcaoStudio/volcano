use std::collections::VecDeque;
use std::time::Instant;
use std::{pin::Pin, sync::Arc};
use std::future::Future;
use async_trait::async_trait;
use bytes::Buf;
use webrtc::sdp::extmap;
use tokio::sync::Mutex;
use tokio::time::{sleep, Duration};
use webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use webrtc::rtcp::transport_feedbacks::transport_layer_nack::TransportLayerNack;
use webrtc::rtp::extension::audio_level_extension::AudioLevelExtension;
use webrtc::rtp::packet::Packet;
use webrtc::util::Unmarshal;

use super::bucket::{self, Bucket};
use super::error::{BufferError, Result};
use super::nack::NackQueue;
use webrtc::rtp_transceiver as rtp;
use rtp::rtp_codec::{RTCRtpParameters, RTPCodecType};
use webrtc::rtcp::packet::Packet as RtcpPacket;

const MAX_SEQUENCE_NUMBER: u32 = 1 << 16;
const REPORT_DELTA: f64 = 1e9;

pub type OnCloseFn =
    Box<dyn (FnMut() -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'static>>) + Send + Sync>;
pub type OnTransportWideCCFn = Box<
    dyn (FnMut(u16, i64, bool) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>) + Send + Sync,
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
    async fn write(&self, pkt: Packet) -> Result<u32>;
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
    arrival_time: u64,
    pub packet: Packet,
}
#[derive(Debug, Eq, PartialEq, Default, Clone)]
pub struct ExtPacket {
    pub head: bool,
    cycle: u32,
    pub arrival: i64,
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
    last_report: i64,
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

    min_packet_probe: i32,
    last_packet_read: i32,

    pub max_temporal_layer: i32,
    pub bitrate: u64,
    bitrate_helper: u64,
    last_srntp_time: u64,
    last_srrtp_time: u32,
    last_sr_recv: i64, // Represents wall clock of the most recent sender report arrival
    base_sn: u16,
    cycles: u32,
    #[allow(dead_code)]
    last_rtcp_packet_time: i64, // Time the last RTCP packet was received.
    #[allow(dead_code)]
    last_rtcp_sr_time: i64, // Time the last RTCP SR was received. Required for DLSR computation.
    last_transit: u32,
    max_seq_no: u16, // The highest sequence number received in an RTP data packet

    stats: Stats,
    latest_timestamp: u32,      // latest received RTP timestamp on packet
    latest_timestamp_time: i64, // Time of the latest timestamp (in nanos since unix epoch)

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
}

pub struct AtomicBuffer {
    buffer: Arc<Mutex<Buffer>>,
}

#[async_trait]
impl BufferIO for AtomicBuffer {
     /// Adds a RTP Packet, out of order, new packet may be arrived later
     async fn write(&self, pkt: Packet) -> Result<u32> {
        {
            let mut buffer = self.buffer.lock().await;

            if !buffer.bound {
                buffer.pending_packets.push(PendingPackets {
                    arrival_time: Instant::now().elapsed().subsec_nanos() as u64,
                    packet: pkt.clone(),
                });

                return Ok(0);
            }
        }

        self.calc(pkt, Instant::now().elapsed().subsec_nanos() as i64)
            .await;

        Ok(0)
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
        let buffer = self.buffer.lock().await;
        if buffer.bucket.is_some() && buffer.codec_type == RTPCodecType::Video {}

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

        debug!("bind -> Processing {} packets", buffer.pending_packets.len());
        for pp in buffer.pending_packets.clone() {
            self.calc(pp.packet, pp.arrival_time as i64).await;
        }
        debug!("bind -> Binding done");

        buffer.pending_packets.clear();
        buffer.bound = true;
    }
    
    pub async fn bitrate(&self) -> u64 {
        self.buffer.lock().await.bitrate
    }

    async fn build_nack_packet(
        &self,
        buffer: &mut Buffer,
    ) -> Vec<Box<dyn RtcpPacket + Send + Sync>> {
        let mut pkts: Vec<Box<dyn RtcpPacket + Send + Sync>> = Vec::new();

        if buffer.nacker == None {
            return pkts;
        }
        let seq_number = buffer.cycles | buffer.max_seq_no as u32;
        let (nacks, ask_key_frame) = buffer.nacker.as_mut().unwrap().pairs(seq_number);

        let mut nacks_len: usize = 0;
        if nacks.is_some() {
            nacks_len = nacks.as_ref().unwrap().len();
        }

        if nacks_len > 0 || ask_key_frame {
            if nacks_len > 0 {
                let pkt = TransportLayerNack {
                    media_ssrc: buffer.media_ssrc,
                    nacks: nacks.unwrap(),
                    ..Default::default()
                };
                pkts.push(Box::new(pkt));
            }

            if ask_key_frame {
                let pkt = PictureLossIndication {
                    media_ssrc: buffer.media_ssrc,
                    ..Default::default()
                };
                pkts.push(Box::new(pkt));
            }
        }

        pkts
    }

    pub async fn calc(&self, packet: Packet, arrival_time: i64) {
        //let codc_type = self.buffer.lock().await.codec_type; //= RTPCodecType::Video;
        let buffer = &mut self.buffer.lock().await;
        let sn = packet.header.sequence_number;
        let distance = bucket::distance(sn, buffer.max_seq_no);

        if buffer.stats.packet_count == 0 {
            buffer.base_sn = sn;
            buffer.max_seq_no = sn;
            buffer.last_report = arrival_time;
        } else if distance & 0x8000 == 0 {
            if sn < buffer.max_seq_no {
                buffer.cycles += MAX_SEQUENCE_NUMBER;
            }
            if buffer.nack {
                let diff = sn - buffer.max_seq_no;

                for i in 1..diff {
                    let msn = sn - i;

                    let ext_sn: u32 = if msn > buffer.max_seq_no
                        && (msn & 0x8000) > 0
                        && buffer.max_seq_no & 0x8000 == 0
                    {
                        (buffer.cycles - MAX_SEQUENCE_NUMBER) | msn as u32
                    } else {
                        buffer.cycles | msn as u32
                    };

                    if let Some(nacker) = buffer.nacker.as_mut() {
                        nacker.push(ext_sn);
                    }
                }
            }
            buffer.max_seq_no = sn;
        } else if buffer.nack && (distance & 0x8000 > 0) {
            let ext_sn: u32 =
                if sn > buffer.max_seq_no && (sn & 0x8000) > 0 && buffer.max_seq_no & 0x8000 == 0 {
                    (buffer.cycles - MAX_SEQUENCE_NUMBER) | sn as u32
                } else {
                    buffer.cycles | sn as u32
                };
            if let Some(nacker) = buffer.nacker.as_mut() {
                nacker.remove(ext_sn);
            }
        }

        let content = packet.to_string();
        let pkt = content.as_bytes();
        let max_seq_no = buffer.max_seq_no;
        if let Some(bucket) = &mut buffer.bucket {
            let rv = bucket.add_packet(pkt, sn, sn == max_seq_no);
            if let Err(err) = rv {
                error!("{err}");
            }
            /* 
            match rv {
                Ok(data) => match Packet::unmarshal(&mut &data[..]) {
                    Err(_) => {
                        return;
                    }
                    Ok(p) => {
                        if codc_type == RTPCodecType::Video {
                            debug!("calc packet size: {}", data.len());
                        }
                        packet = p;
                    }
                },
                Err(err) => {
                    //  if Error::ErrRTXPacket.equal(&rv) {
                    error!("add packet err: {}", err);
                    return;
                }
            }*/
        }

        buffer.stats.total_byte += pkt.len() as u64;
        buffer.bitrate_helper += pkt.len() as u64;
        buffer.stats.packet_count += 1;

        let mut ep = ExtPacket {
            head: sn == buffer.max_seq_no,
            cycle: buffer.cycles,
            packet: packet.clone(),
            arrival: arrival_time,
            key_frame: false,
            ..Default::default()
        };

        match buffer.mime.as_str() {
            "video/vp8" => {
                let mut vp8_packet = VP8::default();
                if let Err(e) = vp8_packet.unmarshal(&packet.payload[..]) {
                    error!("calc error: {}", e);
                    return;
                }
                ep.key_frame = vp8_packet.is_key_frame;
                ep.payload = vp8_packet;
            }
            "video/h264" => {
                ep.key_frame = is_h264_keyframe(&packet.payload[..]);
            }
            _ => {}
        }

        if buffer.min_packet_probe < 25 {
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
        let latest_timestamp_in_nanos_since_epoch = buffer.latest_timestamp_time;
        if (latest_timestamp_in_nanos_since_epoch == 0)
            || is_later_timestamp(packet.header.timestamp, latest_timestamp)
        {
            buffer.latest_timestamp = packet.header.timestamp;
            buffer.latest_timestamp_time = arrival_time;
        }

        let arrival = arrival_time as f64 / 1e6 * (buffer.clock_rate as f64 / 1e3);
        let transit = arrival - packet.header.timestamp as f64;
        if buffer.last_transit != 0 {
            let mut d = transit - buffer.last_transit as f64;
            if d < 0.0 {
                d *= -1.0;
            }

            buffer.stats.jitter += (d - buffer.stats.jitter) / 16_f64;
        }
        buffer.last_transit = transit as u32;

        if buffer.twcc {
            if let Some(mut ext) = packet.header.get_extension(buffer.twcc_ext) {
                if ext.len() > 1 {
                    let mut handler = buffer.on_transport_wide_cc_handler.lock().await;
                    if let Some(f) = &mut *handler {
                        f(ext.get_u16(), arrival_time, packet.header.marker).await;
                    }
                }
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

        let diff = arrival_time - buffer.last_report;

        if buffer.nacker.is_some() {
            let rv = self.build_nack_packet(buffer).await;
            let mut handler = buffer.on_feedback_callback_handler.lock().await;
            if let Some(f) = &mut *handler {
                f(rv).await;
            }
        }

        if diff as f64 >= REPORT_DELTA {
            let br = 8 * buffer.bitrate_helper * REPORT_DELTA as u64 / diff as u64;
            buffer.bitrate = br;
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
                let ext_pkt = ext_packets.pop_front().unwrap();
                return Ok(ext_pkt);
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
            return Err(BufferError::ErrNilPacket.into());
        }

        let mut idx: usize = 0;
        let s = payload[idx] & 0x10 > 0;
        if payload[idx] & 0x80 > 0 {
            idx += 1;

            if payload_len < idx + 1 {
                return Err(BufferError::ErrShortPacket.into());
            }

            self.temporal_supported = payload[idx] & 0x20 > 0;
            let k = payload[idx] & 0x10 > 0;
            let l = payload[idx] & 0x40 > 0;

            // Check for PictureID
            if payload[idx] & 0x80 > 0 {
                idx += 1;
                if payload_len < idx + 1 {
                    return Err(BufferError::ErrShortPacket.into());
                }
                self.picture_id_idx = idx as i32;
                let pid = payload[idx] & 0x7f;
                // Check if m is 1, then Picture ID is 15 bits
                if payload[idx] & 0x80 > 0 {
                    idx += 1;
                    if payload_len < idx + 1 {
                        return Err(BufferError::ErrShortPacket.into());
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
                    return Err(BufferError::ErrShortPacket.into());
                }
                self.tlz_idx = idx as i32;

                if idx >= payload_len {
                    return Err(BufferError::ErrShortPacket.into());
                }
                self.tl0_picture_idx = payload[idx];
            }

            if self.temporal_supported || k {
                idx += 1;
                if payload_len < idx + 1 {
                    return Err(BufferError::ErrShortPacket.into());
                }
                self.tid = (payload[idx] & 0xc0) >> 6;
            }

            if idx >= payload_len {
                return Err(BufferError::ErrShortPacket.into());
            }
            idx += 1;
            if payload_len < idx + 1 {
                return Err(BufferError::ErrShortPacket.into());
            }
            // Check is packet is a keyframe by looking at P bit in vp8 payload
            self.is_key_frame = payload[idx] & 0x01 == 0 && s;
        } else {
            idx += 1;
            if payload_len < idx + 1 {
                return Err(BufferError::ErrShortPacket.into());
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