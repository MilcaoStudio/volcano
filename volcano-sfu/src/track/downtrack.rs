use async_trait::async_trait;
use bytes::Bytes;
use std::any::Any;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Weak};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};
use std::time::Duration;
use tokio::time::Instant;
use webrtc::error::Result as RTCResult;
use webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use webrtc::rtcp::source_description::SourceDescriptionChunk;
use webrtc::rtp;
use webrtc::rtp_transceiver::RTCRtpTransceiver;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecParameters;
use webrtc::rtp_transceiver::rtp_codec::RTPCodecType;
use webrtc::track::track_local::TrackLocal;
use webrtc::track::track_local::TrackLocalContext;
use webrtc::track::track_local::TrackLocalWriter;
use tokio::sync::{Mutex, RwLock};

use crate::packet::{AtomicFactory, ExtPacket};
use crate::track::receiver::WebRTCReceiver;

use super::codec_parameters_fuzzy_search;
use super::error::{Error, Result};
use super::receiver::Receiver;
use super::sequencer::AtomicSequencer;
use super::set_vp8_temporal_layer;
use super::simulcast::SimulcastTrackHelpers;

pub type OnCloseFn =
    Box<dyn (Fn() -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>) + Send + Sync>;

pub type OnBindFn =
    Box<dyn (Fn() -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>) + Send + Sync>;

#[derive(Default, Clone)]
pub enum DownTrackType {
    #[default]
    /// Simple down track. It should be assigned to layer 0.
    SimpleDownTrack,
    /// Simulcast down track. It should be assigned in a layer between 0 and 2.
    SimulcastDownTrack,
}

impl Debug for DownTrackType {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            DownTrackType::SimpleDownTrack => f.write_str("Simple"),
            DownTrackType::SimulcastDownTrack => f.write_str("Simulcast"),
        }
    }
}

#[derive(Default, Clone)]
pub struct DownTrackInfo {
    pub layer: u8,
    pub last_ssrc: u32,
    pub track_type: DownTrackType,
    pub payload: Vec<u8>,
}

pub struct DownTrackInternal {
    id: String,
    rid: String,
    bound: AtomicBool,
    mime: RwLock<String>,
    ssrc: Arc<AtomicU32>,
    stream_id: String,
    max_sn: u16,
    payload_type: AtomicU8,
    sequencer: Arc<RwLock<Arc<AtomicSequencer>>>,
    buffer_factory: Arc<AtomicFactory>,
    /// Whether the track is enabled (unmuted).
    enabled: Arc<AtomicBool>,
    /// Whether the track is re-synced.
    re_sync: Arc<AtomicBool>,
    last_ssrc: Arc<AtomicU32>,
    /// Codec capability of the track.
    media_codec: RTCRtpCodecCapability,
    /// Receiver of the track.
    receiver: Weak<WebRTCReceiver>,
    write_stream: RwLock<Option<Arc<dyn TrackLocalWriter + Send + Sync>>>,
    on_bind_handler: Arc<RwLock<Option<OnBindFn>>>,
}

impl DownTrackInternal {
    pub(crate) fn new(c: RTCRtpCodecCapability, r: &Arc<WebRTCReceiver>, max_track: u16, factory: Arc<AtomicFactory>) -> Self {
        Self {
            media_codec: c,
            id: r.track_id(),
            rid: r.track_rid(),
            bound: AtomicBool::default(),
            mime: Default::default(),
            ssrc: Default::default(),
            stream_id: r.stream_id(),
            max_sn: max_track,
            payload_type: Default::default(),
            sequencer: RwLock::new(Arc::new(AtomicSequencer::new(max_track))).into(),
            buffer_factory: factory,
            enabled: Default::default(),
            re_sync: Default::default(),
            last_ssrc: Default::default(),
            receiver:  Arc::downgrade(r),
            write_stream: Default::default(),
            on_bind_handler: Arc::default(),
        }
    }

    /// Registers a function to be called when the track is bound.
    pub async fn on_bind(&self, f: OnBindFn) {
        let mut handler = self.on_bind_handler.write().await;
        *handler = Some(f);
    }
}

impl PartialEq for DownTrackInternal {
    fn eq(&self, other: &Self) -> bool {
        (self.stream_id == other.stream_id) && (self.id == other.id)
    }
}

#[async_trait]
impl TrackLocal for DownTrackInternal {
    async fn bind(&self, t: &TrackLocalContext) -> RTCResult<RTCRtpCodecParameters> {
        let parameters = RTCRtpCodecParameters {
            capability: self.media_codec.clone(),
            ..Default::default()
        };

        let codec = codec_parameters_fuzzy_search(parameters, t.codec_parameters())?;
        self.ssrc.store(t.ssrc(), Ordering::Release);
        self.payload_type.store(codec.payload_type, Ordering::Release);
        self.re_sync.store(true, Ordering::Release);
        self.enabled.store(true, Ordering::Release);
        
        {
            let mut write_stream = self.write_stream.write().await;
            *write_stream = Some(t.write_stream());
        }
        
        {
            let mut mime = self.mime.write().await;
            *mime = codec.capability.mime_type.to_lowercase();
        }

        let rtcp = self.buffer_factory.get_or_new_rtcp_buffer(t.ssrc());

        let sequencer = self.sequencer.clone();
        let receiver = self.receiver.clone();
        let enabled_out = self.enabled.clone();
        let last_ssrc_out = self.last_ssrc.clone();
        let ssrc_out = self.ssrc.clone();

        rtcp.set_on_packets(Box::new(move |pkts| {
            let sqncr_in = sequencer.clone();
            let rcvr_in = receiver.clone();
            let enabled = enabled_out.clone();
            let last_ssrc_in = last_ssrc_out.clone();
            let ssrc_in = ssrc_out.clone();
            Box::pin(async move {
                if let Some(receiver) = rcvr_in.upgrade() {
                    if enabled.load(Ordering::Acquire) {
                        let sequencer = sqncr_in.read().await;
                        let last_ssrc = last_ssrc_in.load(Ordering::Acquire);
                        let ssrc = ssrc_in.load(Ordering::Acquire);
                        receiver.send_feedback_rtcp(pkts, last_ssrc, ssrc, sequencer.clone()).await;
                    } else {
                        trace!("[Track {}] Cannot send RTCP because track is muted.", receiver.track_id())
                    }
                } else {
                    debug!("RTCP forwarding disabled. WebRTCReceiver dropped.");
                }
            })
        }))
        .await;

        if self.media_codec.mime_type.starts_with("video/") {
            let mut sequencer = self.sequencer.write().await;
            *sequencer = AtomicSequencer::new(self.max_sn).into();
        }

        self.bound.store(true, Ordering::Relaxed);
        let on_bind_handler = self.on_bind_handler.clone();
        tokio::spawn(async move {
            if let Some(handler) = on_bind_handler.read().await.as_ref() {
                handler().await;
            }
        });
        Ok(codec)
    }

    async fn unbind(&self, _t: &TrackLocalContext) -> RTCResult<()> {
        self.bound.store(false, Ordering::Relaxed);
        Ok(())
    }

    fn id(&self) -> &str {
        &self.id
    }

    fn stream_id(&self) -> &str {
        &self.stream_id
    }

    fn kind(&self) -> RTPCodecType {
        if self.media_codec.mime_type.starts_with("audio/") {
            return RTPCodecType::Audio;
        }

        if self.media_codec.mime_type.starts_with("video/") {
            return RTPCodecType::Video;
        }

        RTPCodecType::Unspecified
    }

    fn rid(&self) -> Option<&str> {
        Some(&self.rid)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

pub struct RtxEncoding {
    pub ssrc: u32,
    pub sequencer: AtomicSequencer, 
}

impl RtxEncoding {
    pub fn new(ssrc: u32, max_sn: u16) -> Self {
        Self {
            ssrc,
            sequencer: AtomicSequencer::new(max_sn),
        }
    }
}

pub struct DownTrack {
    /// Canonical name defined by RFC 3550 6.5.1
    cname: String,
    /// Close flag
    closed: AtomicBool,
    //peer_id: String,
    track_type: Mutex<DownTrackType>,
    /// Packet payload stored in buffer.
    pub payload: Vec<u8>,

    current_spatial_layer: AtomicU8,
    target_spatial_layer: AtomicU8,
    /// Temporal layer of the track. Always 0 for simple tracks.
    ///
    /// Highest 2 bytes represent a target layer, and the lowest 2 bytes represent the current layer.
    /// # Example
    /// ```no_run
    /// let tid = 0u16;
    /// let current_layer = layer as u16;
    /// let current_target_layer = (layer >> 16) as u16;
    ///
    /// if current_target_layer != current_layer {
    ///     if tid <= current_target_layer {
    ///         downtrack.temporal_layer.store(
    ///             ((current_target_layer as u32) << 16) | current_target_layer as u32,
    ///             Ordering::Relaxed,
    ///         )
    ///     }
    /// }
    /// ```
    pub temporal_layer: AtomicU32,

    /// Offset from last packet's SN
    sn_offset: Mutex<u16>,
    /// Offset from last packet's timestamp
    ts_offset: Mutex<u32>,

    last_sn: Mutex<u16>,
    last_ts: Mutex<u32>,

    /// Simulcast track helpers.
    pub simulcast: Arc<Mutex<SimulcastTrackHelpers>>,
    /// Maximum spatial layer of the track.
    pub max_spatial_layer: AtomicU8,
    /// Maximum temporal layer of the track.
    pub max_temporal_layer: AtomicU32,
    /// RTCP transceiver of the track.
    pub transceiver: RwLock<Option<Arc<RTCRtpTransceiver>>>,
    on_close_handler: Arc<Mutex<Option<OnCloseFn>>>,

    octet_count: AtomicU32,
    packet_count: AtomicU32,
    packet_sent_count: AtomicU32,
    last_stats: Mutex<Instant>,
    down_track_local: Arc<DownTrackInternal>,

    rtx: Arc<RwLock<Option<Arc<RtxEncoding>>>>,
}

impl DownTrack {
    pub fn new(
        c: RTCRtpCodecCapability,
        r: &Arc<WebRTCReceiver>,
        cname: String,
        max_track: u16,
        factory: Arc<AtomicFactory>,
    ) -> Self {
        Self {
            cname,
            closed: Default::default(),
            track_type: Mutex::new(DownTrackType::SimpleDownTrack),
            payload: Vec::new(),
            current_spatial_layer: AtomicU8::default(),
            target_spatial_layer: AtomicU8::default(),
            temporal_layer: AtomicU32::default(),
            sn_offset: Mutex::default(),
            ts_offset: Mutex::default(),
            last_sn: Mutex::default(),
            last_ts: Mutex::default(),
            simulcast: Arc::new(Mutex::new(SimulcastTrackHelpers::new())),
            max_spatial_layer: AtomicU8::default(),
            max_temporal_layer: AtomicU32::default(),
            transceiver: Default::default(),
            on_close_handler: Arc::default(),
            //close_once: Once::new(),
            octet_count: AtomicU32::default(),
            packet_count: AtomicU32::default(),
            //max_packet_ts: 0,
            down_track_local: Arc::new(DownTrackInternal::new(c, r, max_track, factory)),
            packet_sent_count: AtomicU32::default(),
            last_stats: Instant::now().into(),
            rtx: Default::default(),
        }
    }

    /// Returns true if the track is bound to a receiver.
    pub fn bound(&self) -> bool {
        self.down_track_local.bound.load(Ordering::Relaxed)
    }

    /// Calls the close handler
    pub async fn close(&self) {
        let mut handler = self.on_close_handler.lock().await;
        if let Some(f) = &mut *handler {
            f().await;
        }
    }

    /// Creates a source description chunk for the track.
    /// # Returns
    /// A vector of source description chunks, may be empty
    pub fn create_source_description_chunks(&self) -> Vec<SourceDescriptionChunk> {
        use webrtc::rtcp::source_description::{SourceDescriptionItem, SdesType::SdesCname};

        if !self.bound() {
            return Vec::default();
        }
        
        let ssrc = self.ssrc();

        vec![
            SourceDescriptionChunk {
                source: ssrc,
                items: vec![SourceDescriptionItem {
                    sdes_type: SdesCname,
                    text: Bytes::copy_from_slice(self.cname.as_bytes()),
                }]
            }
        ]
    }

    /// Current spatial layer of the track.
    pub fn current_spatial_layer(&self) -> u8 {
        self.current_spatial_layer.load(Ordering::Acquire)
    }

    /// ID of the track.
    pub fn id(&self) -> String {
        self.down_track_local.id.clone()
    }

    pub fn kind(&self) -> RTPCodecType {
        self.down_track_local.kind()
    }

    /// Mime type of the track.
    pub async fn mime(&self) -> String {
        self.down_track_local.mime.read().await.clone()
    }

    /// # Arguments
    /// - `val`: If `true`, track is muted. Otherwise, track is enabled.
    pub fn mute(&self, val: bool) {
        if self.down_track_local.enabled.load(Ordering::Relaxed) != val {
            return;
        }
        self.down_track_local.enabled.store(!val, Ordering::Relaxed);
        if val {
            self.down_track_local.re_sync.store(val, Ordering::Relaxed);
        }
    }

    /// Creates a new simple `DownTrack` from a `DownTrackInternal` and the ID of the peer where this belongs to.
    pub fn new_track_local(cname: String, track: Arc<DownTrackInternal>) -> Self {
        Self {
            cname,
            closed: Default::default(),
            track_type: Mutex::new(DownTrackType::SimpleDownTrack),
            payload: Vec::default(),

            current_spatial_layer: AtomicU8::default(),
            target_spatial_layer: AtomicU8::default(),
            temporal_layer: AtomicU32::default(),

            sn_offset: Mutex::default(),
            ts_offset: Mutex::default(),

            last_sn: Mutex::default(),
            last_ts: Mutex::default(),

            simulcast: Arc::new(Mutex::new(SimulcastTrackHelpers::new())),
            max_spatial_layer: AtomicU8::default(),
            max_temporal_layer: AtomicU32::default(),

            transceiver: Default::default(),
            on_close_handler: Arc::default(),
            //close_once: Once::new(),
            octet_count: AtomicU32::default(),
            packet_count: AtomicU32::default(),
            //max_packet_ts: 0,
            down_track_local: track,
            packet_sent_count: AtomicU32::default(),
            last_stats: Instant::now().into(),
            rtx: Default::default(),
        }
    }

    /// Payload type of the track.
    pub fn payload_type(&self) -> u8 {
        self.down_track_local.payload_type.load(Ordering::Acquire)
    }

    /// Registers a function to be called when the track is bound.
    /// Alias for [DownTrackInternal::on_bind].
    pub async fn on_bind(&self, f: OnBindFn) {
        self.down_track_local.on_bind(f).await
    }

    /// Registers a function to be called when [Self::close] is called.
    pub async fn on_close(&self, f: OnCloseFn) {
        // Asserts downtrack is closed
        if self.closed.swap(true, Ordering::Relaxed) {
            return;
        }
        let mut h = self.on_close_handler.lock().await;
        *h = Some(f);
    }

    /// Sets the initial layers of the track.
    /// # Example
    /// ```no_run
    /// use std::sync::Arc;
    /// use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
    /// use volcano_sfu::track::downtrack::{DownTrack, DownTrackInternal};
    ///
    /// let local_track = Arc::new(
    ///     DownTrackInternal::new(RTCRtpCodecCapability::default(), receiver, 500)
    /// );
    /// let down_track = DownTrack::new_track_local("test".to_owned(), local_track);
    /// down_track.set_initial_layers(0, 0);
    /// ```
    pub fn set_initial_layers(&self, spatial_layer: u8, temporal_layer: u32) {
        self.current_spatial_layer
            .store(spatial_layer, Ordering::Release);
        self.target_spatial_layer
            .store(spatial_layer, Ordering::Release);
        self.temporal_layer.store(temporal_layer, Ordering::Release);
    }

    /// Atomically sets the last SSRC of the track.
    pub fn set_last_ssrc(&self, val: u32) {
        self.down_track_local
            .last_ssrc
            .store(val, Ordering::Release);
    }

    /// Atomically sets the maximum spatial layer of the track.
    pub fn set_max_spatial_layer(&self, val: u8) {
        self.max_spatial_layer.store(val, Ordering::Release);
    }

    /// Atomically sets the maximum temporal layer of the track.
    pub fn set_max_temporal_layer(&self, val: u32) {
        self.max_temporal_layer.store(val, Ordering::Release);
    }

    /// Sets the [DownTrackType] of the track.
    pub async fn set_track_type(&self, track_type: DownTrackType) {
        *self.track_type.lock().await = track_type;
    }

    /// Sets the transceiver of the track.
    pub async fn set_transceiver(&self, transceiver: Arc<RTCRtpTransceiver>) {
        let rtx = transceiver.sender().await.get_parameters().await.encodings.iter().find_map(|e| {
            if e.rtx.ssrc > 0 { Some(Arc::new(RtxEncoding::new(e.rtx.ssrc, 1_000))) } else { None }
        });
        *self.transceiver.write().await = Some(transceiver);
        *self.rtx.write().await = rtx;
    }

    /// SSRC of the track.
    pub fn ssrc(&self) -> u32 {
        self.down_track_local.ssrc.load(Ordering::Acquire)
    }

    /// Atomically sets the spatial layer of the track.
    /// # Arguments
    /// - `target_layer`: Target spatial layer.
    /// - `set_as_max`: If true, sets the target layer as the maximum spatial layer.
    /// # Errors
    /// - `Error::ReceiverLayerNotAvailable(target)` if the target layer is higher than max.
    pub async fn set_target_spatial_layer(
        self: &Arc<Self>,
        target_layer: u8,
        set_as_max: bool,
    ) -> Result<()> {
        match *self.track_type.lock().await {
            DownTrackType::SimulcastDownTrack => {
                let current = self.current_spatial_layer.load(Ordering::Acquire);
                // Case 1: current is the target
                if current == target_layer {
                    if set_as_max {
                        let _ = self.max_spatial_layer.fetch_update(
                            Ordering::SeqCst, 
                            Ordering::SeqCst,
                            |current_max| Some(current_max.max(target_layer))
                        );
                    }
                    return Ok(());
                }
                
                // Validation
                let max = self.max_spatial_layer.load(Ordering::Acquire);
                if max < target_layer {
                    if set_as_max {
                        self.set_max_spatial_layer(target_layer);
                    } else {
                        // Target should not be higher than max
                        return Err(Error::ReceiverLayerNotAvailable(target_layer as usize));
                    }
                }

                // Update
                self.target_spatial_layer.store(target_layer, Ordering::Release);
                return Ok(());
            }
            _ => {
                debug!("switch_spatial_layer Simple track cannot switch layer");
            }
        }

        Err(Error::ErrInvalidTrack)
    }

    pub fn stream_id(&self) -> String {
        self.down_track_local.stream_id.clone()
    }

    /// Atomically switches the spatial layer of the track.
    /// No checks in the target layer are performed.
    /// # Arguments
    /// - `layer`: Target spatial layer.
    pub fn switch_spatial_layer_forced(&self, layer: u8) {
        self.current_spatial_layer.store(layer, Ordering::Release);
    }

    /// Atomically switches the temporal layer of the track.
    /// Not working for simple tracks.
    /// # Arguments
    /// - `target_layer`: Target temporal layer.
    /// - `set_as_max`: If true, sets the target layer as the maximum temporal layer.
    pub async fn switch_temporal_layer(&self, target_layer: u32, set_as_max: bool) {
        match *self.track_type.lock().await {
            DownTrackType::SimulcastDownTrack => {
                let layer = self.temporal_layer.load(Ordering::Acquire);

                if layer == target_layer {
                    return;
                }

                self.temporal_layer.store(target_layer, Ordering::Release);

                if set_as_max {
                    self.max_temporal_layer
                        .store(target_layer, Ordering::Release);
                }
                info!(
                    "[Track {}] Temporal layer: {target_layer} (max={set_as_max})",
                    self.id()
                );
            }

            _ => {
                info!("switch_temporal_layer Simple track cannot switch layer");
            }
        }
    }

    /// Requests the track to update its stats.
    /// # Arguments
    /// - `packet_len`: Length of the packet.
    pub fn update_stats(&self, packet_len: u32) {
        self.octet_count.store(packet_len, Ordering::Relaxed);
        self.packet_count.store(1, Ordering::Relaxed);
    }

    /// Writes a raw RTP packet directly to the track.
    pub async fn write_raw_rtp(&self, pkt: rtp::packet::Packet) -> Result<()> {
        let write_stream_val = self.down_track_local.write_stream.read().await;
        if let Some(write_stream) = write_stream_val.as_ref() {
            write_stream.write_rtp(&pkt).await?;
        }

        Ok(())
    }

    /// Forwards an Extended Packet from a track receiver to the local track.
    /// # Arguments
    /// - `pkt`: Extended packet.
    /// - `layer`: Layer of the track (ignored for simple tracks).
    pub async fn forward_rtp(&self, pkt: &ExtPacket, layer: usize) -> Result<()> {
        if !self.down_track_local.enabled.load(Ordering::Relaxed) {
            return Ok(());
        }
        if !self.bound() {
            return Ok(());
        }

        let layer_u8 = (layer & 0xFF) as u8;

        let mut packet = pkt.clone();
        let res = match *self.track_type.lock().await {
            DownTrackType::SimpleDownTrack => self.forward_simple_rtp(&mut packet).await,
            DownTrackType::SimulcastDownTrack => self.forward_simulcast_rtp(&mut packet, layer_u8).await,
        };

        let seconds_per_stat = 20;
        if res.is_ok() {
            let mut instant = self.last_stats.lock().await;
            if instant.elapsed() >= Duration::from_secs(seconds_per_stat as u64) {
                let count = self
                    .packet_sent_count
                    .swap(0, Ordering::SeqCst) / seconds_per_stat;
                trace!(
                    "[Track {}] Sending {count} RTP packets per second",
                    self.id()
                );
                *instant = Instant::now();
            }
        }

        res
    }

    async fn forward_simple_rtp(&self, ext_packet: &mut ExtPacket) -> Result<()> {
        let ssrc = self.ssrc();

        if self.down_track_local.re_sync.load(Ordering::Relaxed) {
            match self.down_track_local.kind() {
                RTPCodecType::Video => {
                    if !ext_packet.key_frame {
                        if let Some(receiver) =  &self.down_track_local.receiver.upgrade() {
                            let media_ssrc = ext_packet.packet.header.ssrc;
                            receiver.send_pli(ssrc, media_ssrc).await;
                        }
                        return Ok(());
                    }
                }
                RTPCodecType::Audio => {
                    trace!(
                        "[Downtrack {}] Writing RTP in audio track (simple)",
                        self.id()
                    );
                }
                kind => {
                    trace!(
                        "[Downtrack {}] Writing RTP in {} track (simple)",
                        self.id(),
                        kind
                    );
                }
            }

            {
                let last_sn = *self.last_sn.lock().await;
                if last_sn != 0 {
                    let mut sn_offset = self.sn_offset.lock().await;
                    *sn_offset = ext_packet
                        .packet
                        .header
                        .sequence_number
                        .saturating_sub(last_sn)
                        .saturating_sub(1);

                    let last_ts = *self.last_ts.lock().await;
                    let mut ts_offset = self.ts_offset.lock().await;
                    *ts_offset = ext_packet
                        .packet
                        .header
                        .timestamp
                        .saturating_sub(last_ts)
                        .saturating_sub(1);
                }
            }

            self.down_track_local
                .last_ssrc
                .store(ext_packet.packet.header.ssrc, Ordering::Relaxed);

            self.down_track_local
                .re_sync
                .store(false, Ordering::Relaxed);
        }
        self.update_stats(ext_packet.packet.payload.len() as u32);

        {
            let new_sn = ext_packet
                .packet
                .header
                .sequence_number
                .wrapping_sub(*self.sn_offset.lock().await);
            let new_ts = ext_packet
                .packet
                .header
                .timestamp
                .saturating_sub(*self.ts_offset.lock().await);
            let sequencer = self.down_track_local.sequencer.read().await;
            sequencer
                .push(
                    ext_packet.packet.header.sequence_number,
                    new_sn,
                    new_ts,
                    0,
                    ext_packet.head,
                )
                .await;
            let cur_payload_type = self.down_track_local.payload_type.load(Ordering::Acquire);
            if ext_packet.head {
                let mut last_sn = self.last_sn.lock().await;
                *last_sn = new_sn;
                let mut last_ts = self.last_ts.lock().await;
                *last_ts = new_ts;
            }
            let header = &mut ext_packet.packet.header;
            header.payload_type = cur_payload_type;
            header.timestamp = new_ts;
            header.sequence_number = new_sn;
            header.ssrc = ssrc;
        }

        let write_stream_val = self.down_track_local.write_stream.read().await;
        if let Some(write_stream) = &*write_stream_val {
            self.packet_sent_count
                .fetch_add(1, Ordering::SeqCst);
            write_stream.write_rtp(&ext_packet.packet).await?;
        } else {
            error!(
                "[Track {}] No write stream. Packet will be not transmited.",
                self.id()
            );
        }

        Ok(())
    }

    async fn forward_simulcast_rtp(&self, ext_packet: &mut ExtPacket, layer: u8) -> Result<()> {
        let re_sync = self.down_track_local.re_sync.load(Ordering::Relaxed);
        let csl = self.current_spatial_layer();

        if csl != layer {
            return Ok(());
        }

        let ssrc = self.ssrc();

        let last_ssrc = self.down_track_local.last_ssrc.load(Ordering::Relaxed);
        let temporal_supported: bool;
        let pkt_arrival_time = ext_packet.arrival.as_millis() as u32; // expected wrap around 1_193 hours
        trace!("Packet arrival time: {pkt_arrival_time} ms");

        {
            let simulcast = &mut self.simulcast.lock().await;
            temporal_supported = simulcast.temporal_supported;
            if last_ssrc != ext_packet.packet.header.ssrc || re_sync {

                if let Some(receiver) = &self.down_track_local.receiver.upgrade() {
                    if re_sync && !ext_packet.key_frame {
                        receiver
                            .send_rtcp(vec![Box::new(PictureLossIndication {
                                sender_ssrc: ssrc,
                                media_ssrc: ext_packet.packet.header.ssrc,
                            })])
                            .await?;
                        return Ok(());
                    }
                }

                if re_sync && simulcast.l_ts_calc != 0 {
                    simulcast.l_ts_calc = pkt_arrival_time;
                }

                if simulcast.temporal_supported {
                    let mime = self.down_track_local.mime.read().await;
                    if mime.as_str() == "video/vp8" {
                        let vp8 = ext_packet.payload;
                        simulcast.p_ref_pic_id = simulcast.l_pic_id;
                        simulcast.ref_pic_id = vp8.picture_id;
                        simulcast.p_ref_tlz_idx = simulcast.l_tlz_idx;
                        simulcast.ref_tlz_idx = vp8.tl0_picture_idx;
                    }
                }
                self.down_track_local
                    .re_sync
                    .store(false, Ordering::Relaxed);
            }

            if simulcast.l_ts_calc != 0 && last_ssrc != ext_packet.packet.header.ssrc {
                self.down_track_local
                    .last_ssrc
                    .store(ext_packet.packet.header.ssrc, Ordering::Relaxed);
                let tdiff = pkt_arrival_time.saturating_sub(simulcast.l_ts_calc); // ms
                let mut td = (tdiff as u32 * 90) / 1000; // clock cycles
                if td == 0 {
                    td = 1;
                }
                let mut ts_offset = self.ts_offset.lock().await;
                *ts_offset = ext_packet.packet.header.timestamp - (*self.last_ts.lock().await + td);
                trace!("New timestamp offset: {ts_offset}");
                let mut sn_offset = self.sn_offset.lock().await;
                *sn_offset =
                    ext_packet.packet.header.sequence_number - *self.last_sn.lock().await - 1;
            } else if simulcast.l_ts_calc == 0 {
                let mut last_ts = self.last_ts.lock().await;
                *last_ts = ext_packet.packet.header.timestamp;
                let mut last_sn = self.last_sn.lock().await;
                *last_sn = ext_packet.packet.header.sequence_number;
                let mime = self.down_track_local.mime.read().await;
                    if mime.as_str() == "video/vp8" {
                    let vp8 = ext_packet.payload;
                    simulcast.temporal_supported = vp8.temporal_supported;
                }
            }
        }

        let new_sn = ext_packet.packet.header.sequence_number - *self.sn_offset.lock().await;
        let new_ts = ext_packet.packet.header.timestamp - *self.ts_offset.lock().await;
        let payload = &ext_packet.packet.payload;

        let _pic_id: u16 = 0;
        let _tlz0_idx: u8 = 0;

        if temporal_supported {
            let mime = self.down_track_local.mime.read().await;
            if mime.as_str() == "video/vp8" {
                let (_a, _b, _c, _d) = set_vp8_temporal_layer(&ext_packet, self).await;
            }
        }

        self.octet_count
            .fetch_add(payload.len() as u32, Ordering::Relaxed);
        self.packet_count.fetch_add(1, Ordering::Relaxed);

        if ext_packet.head {
            *self.last_sn.lock().await = new_sn;
            *self.last_ts.lock().await = new_ts;
        }
        {
            let simulcast = &mut self.simulcast.lock().await;
            simulcast.l_ts_calc = ext_packet.arrival.as_millis() as u32;
        }

        let hdr = &mut ext_packet.packet.header;
        hdr.sequence_number = new_sn;
        hdr.timestamp = new_ts;
        hdr.ssrc = ssrc;
        hdr.payload_type = self.down_track_local.payload_type.load(Ordering::Acquire);

        let write_stream_val = self.down_track_local.write_stream.read().await;
        if let Some(write_stream) = write_stream_val.as_ref() {
            trace!(
                "Sending packet to write stream (simulcast) [layer {layer}] {:?}",
                ext_packet
            );
            write_stream.write_rtp(&ext_packet.packet).await?;
        } else {
            error!(
                "[Track {}] No write stream. Packet will be not transmited.",
                self.id()
            );
        }

        Ok(())
    }

    pub async fn rtx_encoding(&self) -> Option<Arc<RtxEncoding>> {
        self.rtx.read().await.clone()
    }
}

impl PartialEq for DownTrack {
    fn eq(&self, other: &Self) -> bool {
        (self.cname == other.cname) && (self.down_track_local == other.down_track_local)
    }
}

use std::fmt::{Debug, Formatter};
impl Debug for DownTrack {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DownTrack")
            .field("id", &self.id())
            .field("cname", &self.cname)
            .field("track_type", &self.track_type)
            .field("current_spatial_layer", &self.current_spatial_layer)
            .field("target_spatial_layer", &self.target_spatial_layer)
            .field("temporal_layer", &self.temporal_layer)
            .field("max_spatial_layer", &self.max_spatial_layer)
            .field("max_temporal_layer", &self.max_temporal_layer)
            .finish()
    }
}