use async_trait::async_trait;
use bytes::Bytes;
use std::any::Any;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};
use std::time::Duration;
use tokio::time::Instant;
use webrtc::error::Error as RTCError;
use webrtc::error::Result as RTCResult;
use webrtc::rtcp;
use webrtc::rtcp::packet::Packet as RtcpPacket;
use webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use webrtc::rtcp::source_description::SdesType;
use webrtc::rtcp::source_description::SourceDescriptionChunk;
use webrtc::rtcp::source_description::SourceDescriptionItem;
use webrtc::rtp;
use webrtc::rtp_transceiver::RTCRtpTransceiver;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecParameters;
use webrtc::rtp_transceiver::rtp_codec::RTPCodecType;
use webrtc::track::track_local::TrackLocal;
use webrtc::track::track_local::TrackLocalContext;
use webrtc::track::track_local::TrackLocalWriter;
//use std::sync::Once;
use tokio::sync::Mutex;

use crate::buffer::AtomicFactory;
use crate::buffer::ExtPacket;
use crate::track::receiver::WebRTCReceiver;

use super::codec_parameters_fuzzy_search;
use super::error::{Error, Result};
use super::receiver::Receiver;
use super::sequencer::AtomicSequencer;
use super::sequencer::PacketMeta;
use super::set_vp8_temporal_layer;
use super::simulcast::SimulcastTrackHelpers;

pub type OnCloseFn =
    Box<dyn (FnMut() -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>) + Send + Sync>;

pub type OnBindFn =
    Box<dyn (FnMut() -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>) + Send + Sync>;

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
    mime: Mutex<String>,
    ssrc: Mutex<u32>,
    stream_id: String,
    max_track: u32,
    payload_type: Mutex<u8>,
    sequencer: Arc<Mutex<AtomicSequencer>>,
    buffer_factory: AtomicFactory,
    /// Whether the track is enabled (unmuted).
    pub enabled: AtomicBool,
    /// Whether the track is re-synced.
    pub re_sync: AtomicBool,
    last_ssrc: AtomicU32,
    /// Codec capability of the track.
    pub codec: RTCRtpCodecCapability,
    /// Receiver of the track.
    pub receiver: Arc<WebRTCReceiver>,
    write_stream: Mutex<Option<Arc<dyn TrackLocalWriter + Send + Sync>>>,
    on_bind_handler: Arc<Mutex<Option<OnBindFn>>>,
}

impl DownTrackInternal {
    pub(crate) fn new(c: RTCRtpCodecCapability, r: Arc<WebRTCReceiver>, max_track: u32) -> Self {
        Self {
            codec: c,
            id: r.track_id(),
            rid: r.track_rid(),
            bound: AtomicBool::default(),
            mime: Mutex::default(),
            ssrc: Mutex::default(),
            stream_id: r.stream_id(),
            max_track,
            payload_type: Mutex::default(),
            sequencer: Mutex::new(AtomicSequencer::new(max_track)).into(),
            buffer_factory: AtomicFactory::default(),
            enabled: AtomicBool::default(),
            re_sync: AtomicBool::default(),
            last_ssrc: AtomicU32::default(),
            receiver: r.clone(),
            write_stream: Mutex::default(),
            on_bind_handler: Arc::default(),
        }
    }

    /// Registers a function to be called when the track is bound.
    pub async fn on_bind(&self, f: OnBindFn) {
        let mut handler = self.on_bind_handler.lock().await;
        *handler = Some(f);
    }

    async fn handle_rtcp(
        enabled: bool,
        data: Vec<u8>,
        last_ssrc: u32,
        ssrc: u32,
        sequencer: Arc<Mutex<AtomicSequencer>>,
        receiver: Arc<dyn Receiver>,
    ) {
        if !enabled {
            return;
        }

        let mut buf = &data[..];

        let pkts_result = rtcp::packet::unmarshal(&mut buf);
        let mut pkts;

        match pkts_result {
            Ok(pkts_rv) => {
                pkts = pkts_rv;
            }
            Err(_) => {
                return;
            }
        }

        let mut fwd_pkts: Vec<Box<dyn RtcpPacket + Send + Sync>> = Vec::new();
        let mut pli_once = true;
        let mut fir_once = true;

        let mut max_rate_packet_loss: u8 = 0;
        let mut expected_min_bitrate: u64 = 0;

        if last_ssrc == 0 {
            return;
        }

        for pkt in &mut pkts {
            if let Some(pic_loss_indication) = pkt
                    .as_any()
                    .downcast_ref::<rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication>()
                {
                    if pli_once {
                        let mut pli = pic_loss_indication.clone();
                        pli.media_ssrc = last_ssrc;
                        pli.sender_ssrc = ssrc;

                        fwd_pkts.push(Box::new(pli));
                        pli_once = false;
                    }
                }
            else if let Some(full_intra_request) = pkt
                    .as_any()
                    .downcast_ref::<rtcp::payload_feedbacks::full_intra_request::FullIntraRequest>()
            {
                if fir_once{
                    let mut fir = full_intra_request.clone();
                    fir.media_ssrc = last_ssrc;
                    fir.sender_ssrc = ssrc;

                    fwd_pkts.push(Box::new(fir));
                    fir_once = false;
                }
            }
            else if let Some(receiver_estimated_max_bitrate) = pkt
                    .as_any()
                    .downcast_ref::<rtcp::payload_feedbacks::receiver_estimated_maximum_bitrate::ReceiverEstimatedMaximumBitrate>()
                    {
                if expected_min_bitrate == 0 || expected_min_bitrate > receiver_estimated_max_bitrate.bitrate as u64{
                    expected_min_bitrate = receiver_estimated_max_bitrate.bitrate as u64;

                }
            }
            else if let Some(receiver_report) = pkt.as_any().downcast_ref::<rtcp::receiver_report::ReceiverReport>(){

                for r in &receiver_report.reports{
                    if max_rate_packet_loss == 0 || max_rate_packet_loss < r.fraction_lost{
                        max_rate_packet_loss = r.fraction_lost;
                    }

                }

            }
            else if let Some(transport_layer_nack) = pkt.as_any().downcast_ref::<rtcp::transport_feedbacks::transport_layer_nack::TransportLayerNack>()
            {
                let mut nacked_packets:Vec<PacketMeta> = Vec::new();
                for pair in &transport_layer_nack.nacks {
                    let seq_numbers = pair.packet_list();
                    let sequencer2 = sequencer.lock().await;
                    let mut pairs= sequencer2.get_seq_no_pairs(&seq_numbers[..]).await;
                    nacked_packets.append(&mut pairs);
                }

                warn!("Packet retransmition disabled. Could not retransmit {} packets.", nacked_packets.len());
             //   receiver.retransmit_packets(track, packets)

            }
        }

        if !fwd_pkts.is_empty() {
            if let Err(err) = receiver.send_rtcp(fwd_pkts).await {
                log::error!("send_rtcp err:{}", err);
            }
        }

        // Ok(())
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
            capability: self.codec.clone(),
            ..Default::default()
        };

        let codec = codec_parameters_fuzzy_search(parameters, t.codec_parameters())?;

        let mut ssrc = self.ssrc.lock().await;
        *ssrc = t.ssrc();
        let mut payload_type = self.payload_type.lock().await;
        *payload_type = codec.payload_type;
        let mut write_stream = self.write_stream.lock().await;
        *write_stream = Some(t.write_stream());
        let mut mime = self.mime.lock().await;
        *mime = codec.capability.mime_type.to_lowercase();
        self.re_sync.store(true, Ordering::Relaxed);
        self.enabled.store(true, Ordering::Relaxed);

        let rtcp = self.buffer_factory.get_or_new_rtcp_buffer(t.ssrc()).await;

        let enabled = self.enabled.load(Ordering::Relaxed);
        let last_ssrc = self.last_ssrc.load(Ordering::Relaxed);

        let ssrc_val = *ssrc;

        let sequencer = self.sequencer.clone();
        let receiver = self.receiver.clone();

        rtcp.register_on_packet(Box::new(move |data: Vec<u8>| {
            let sequencer2 = sequencer.clone();
            let receiver2 = receiver.clone();
            Box::pin(async move {
                DownTrackInternal::handle_rtcp(
                    enabled, data, last_ssrc, ssrc_val, sequencer2, receiver2,
                )
                .await;
                Ok(())
            })
        }))
        .await;

        if self.codec.mime_type.starts_with("video/") {
            let mut sequencer = self.sequencer.lock().await;
            *sequencer = AtomicSequencer::new(self.max_track);
        }

        let mut handler = self.on_bind_handler.lock().await;
        if let Some(f) = &mut *handler {
            f().await;
        }
        self.bound.store(true, Ordering::Relaxed);
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
        if self.codec.mime_type.starts_with("audio/") {
            return RTPCodecType::Audio;
        }

        if self.codec.mime_type.starts_with("video/") {
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
pub struct DownTrack {
    peer_id: String,
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

    sn_offset: Mutex<u16>,
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
    pub transceiver: Option<Arc<RTCRtpTransceiver>>,
    on_close_handler: Arc<Mutex<Option<OnCloseFn>>>,

    octet_count: AtomicU32,
    packet_count: AtomicU32,
    packet_sent_count: AtomicU32,
    last_stats: Mutex<Instant>,
    down_track_local: Arc<DownTrackInternal>,
}

impl DownTrack {
    pub fn new(
        c: RTCRtpCodecCapability,
        r: Arc<WebRTCReceiver>,
        peer_id: String,
        max_track: u32,
    ) -> Self {
        Self {
            peer_id,
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
            transceiver: Option::default(),
            on_close_handler: Arc::default(),
            //close_once: Once::new(),
            octet_count: AtomicU32::default(),
            packet_count: AtomicU32::default(),
            //max_packet_ts: 0,
            down_track_local: Arc::new(DownTrackInternal::new(c, r, max_track)),
            packet_sent_count: AtomicU32::default(),
            last_stats: Instant::now().into(),
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
    /// A vector of source description chunks, or None if the track is not bound.
    pub async fn create_source_description_chunks(&self) -> Option<Vec<SourceDescriptionChunk>> {
        if !self.bound() {
            return None;
        }

        let mid = self.transceiver.as_ref().unwrap().mid().unwrap();
        let ssrc = *self.down_track_local.ssrc.lock().await;

        Some(vec![
            SourceDescriptionChunk {
                source: ssrc,
                items: vec![SourceDescriptionItem {
                    sdes_type: SdesType::SdesCname,
                    text: Bytes::copy_from_slice(self.down_track_local.stream_id.as_bytes()),
                }],
            },
            SourceDescriptionChunk {
                source: ssrc,
                items: vec![SourceDescriptionItem {
                    sdes_type: SdesType::SdesCname,
                    text: Bytes::copy_from_slice(mid.as_bytes()),
                }],
            },
        ])
    }

    /// Current spatial layer of the track.
    pub fn current_spatial_layer(&self) -> u8 {
        self.current_spatial_layer.load(Ordering::Acquire)
    }

    /// ID of the track.
    pub fn id(&self) -> String {
        self.down_track_local.id.clone()
    }

    /// Mime type of the track.
    pub async fn mime(&self) -> String {
        self.down_track_local.mime.lock().await.clone()
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
    pub fn new_track_local(peer_id: String, track: Arc<DownTrackInternal>) -> Self {
        Self {
            peer_id,
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

            transceiver: None,
            on_close_handler: Arc::default(),
            //close_once: Once::new(),
            octet_count: AtomicU32::default(),
            packet_count: AtomicU32::default(),
            //max_packet_ts: 0,
            down_track_local: track,
            packet_sent_count: AtomicU32::default(),
            last_stats: Instant::now().into(),
        }
    }

    /// Payload type of the track.
    pub async fn payload_type(&self) -> u8 {
        *self.down_track_local.payload_type.lock().await
    }

    /// Registers a function to be called when the track is bound.
    /// Alias for [DownTrackInternal::on_bind].
    pub async fn register_on_bind(&self, f: OnBindFn) {
        self.down_track_local.on_bind(f).await
    }

    /// Registers a function to be called when [Self::close] is called.
    pub async fn register_on_close(&self, f: OnCloseFn) {
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
    pub fn set_transceiver(&mut self, transceiver: Arc<RTCRtpTransceiver>) {
        self.transceiver = Some(transceiver)
    }

    /// SSRC of the track.
    pub async fn ssrc(&self) -> u32 {
        let ssrc = self.down_track_local.ssrc.lock().await;
        *ssrc
    }

    /// Atomically switches the spatial layer of the track.
    /// # Arguments
    /// - `target_layer`: Target spatial layer.
    /// - `set_as_max`: If true, sets the target layer as the maximum spatial layer.
    /// # Errors
    /// - `Error::FullSpatialLayer` if the target layer is currently full.
    /// - `Error::ErrWebRTC` if the error is from webrtc.
    pub async fn switch_spatial_layer(
        self: &Arc<Self>,
        target_layer: u8,
        set_as_max: bool,
    ) -> Result<()> {
        match *self.track_type.lock().await {
            DownTrackType::SimulcastDownTrack => {
                let csl = self.current_spatial_layer.load(Ordering::Acquire);
                if csl != self.target_spatial_layer.load(Ordering::Acquire) || csl == target_layer {
                    return Err(Error::FullSpatialLayer(target_layer));
                }
                let receiver = &self.down_track_local.receiver;
                match receiver
                    .switch_down_track(self.clone(), target_layer as usize)
                    .await
                {
                    Ok(_) => {
                        self.target_spatial_layer
                            .store(target_layer, Ordering::Release);
                        if set_as_max {
                            self.max_spatial_layer
                                .store(target_layer, Ordering::Release);
                        }
                        debug!(
                            "[Track {}] Switch to spatial layer: {target_layer} (max={set_as_max})",
                            self.id()
                        );
                    }
                    Err(err) => {
                        error!("switch_down_track err: {}", err);
                    }
                }
                return Ok(());
            }
            _ => {
                debug!("switch_spatial_layer Simple track cannot switch layer");
            }
        }

        Err(Error::ErrWebRTC(RTCError::new(
            "Spatial layer is not supported for this track.".to_string(),
        )))
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
        let write_stream_val = self.down_track_local.write_stream.lock().await;
        if let Some(write_stream) = &*write_stream_val {
            write_stream.write_rtp(&pkt).await?;
        }

        Ok(())
    }

    /// Forwards an Extended Packet from a track receiver to the local track.
    /// # Arguments
    /// - `pkt`: Extended packet.
    /// - `layer`: Layer of the track (ignored for simple tracks).
    pub async fn forward_rtp(&self, pkt: ExtPacket, layer: usize) -> Result<()> {
        if !self.down_track_local.enabled.load(Ordering::Relaxed) {
            return Ok(());
        }
        if !self.bound() {
            return Ok(());
        }

        let layer_u8 = (layer & 0xFF) as u8;

        let res = match *self.track_type.lock().await {
            DownTrackType::SimpleDownTrack => self.forward_simple_rtp(pkt).await,
            DownTrackType::SimulcastDownTrack => self.forward_simulcast_rtp(pkt, layer_u8).await,
        };

        let seconds_per_stat = 20;
        if res.is_ok() {
            let mut instant = self.last_stats.lock().await;
            if instant.elapsed() >= Duration::from_secs(seconds_per_stat as u64) {
                let count = self
                    .packet_sent_count
                    .swap(0, Ordering::SeqCst) / seconds_per_stat;
                trace!(
                    "[Track {}] Sending {count} simple RTP packets per second",
                    self.id()
                );
                *instant = Instant::now();
            }
        }

        res
    }

    async fn forward_simple_rtp(&self, packet: ExtPacket) -> Result<()> {
        let mut ext_packet = packet.clone();
        let ssrc = *self.down_track_local.ssrc.lock().await;

        if self.down_track_local.re_sync.load(Ordering::Relaxed) {
            match self.down_track_local.kind() {
                RTPCodecType::Video => {
                    if !ext_packet.key_frame {
                        let receiver = &self.down_track_local.receiver;
                        if !receiver.is_recent_pli().await {
                            let media_ssrc = ext_packet.packet.header.ssrc;
                            debug!("send PLI, ssrc:{}, media_ssrc:{}", ssrc, media_ssrc);
                            receiver
                                .send_rtcp(vec![Box::new(PictureLossIndication {
                                    sender_ssrc: ssrc,
                                    media_ssrc,
                                })])
                                .await?;
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
            let sequencer = self.down_track_local.sequencer.lock().await;
            sequencer
                .push(
                    ext_packet.packet.header.sequence_number,
                    new_sn,
                    new_ts,
                    0,
                    ext_packet.head,
                )
                .await;
            let cur_payload_type = *self.down_track_local.payload_type.lock().await;
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

        let write_stream_val = self.down_track_local.write_stream.lock().await;
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

    async fn forward_simulcast_rtp(&self, ext_packet: ExtPacket, layer: u8) -> Result<()> {
        let re_sync = self.down_track_local.re_sync.load(Ordering::Relaxed);
        let csl = self.current_spatial_layer();

        if csl != layer {
            return Ok(());
        }

        let ssrc = *self.down_track_local.ssrc.lock().await;

        let last_ssrc = self.down_track_local.last_ssrc.load(Ordering::Relaxed);
        let temporal_supported: bool;

        {
            let simulcast = &mut self.simulcast.lock().await;
            temporal_supported = simulcast.temporal_supported;
            if last_ssrc != ext_packet.packet.header.ssrc || re_sync {
                if re_sync && !ext_packet.key_frame {
                    let receiver = &self.down_track_local.receiver;
                    receiver
                        .send_rtcp(vec![Box::new(PictureLossIndication {
                            sender_ssrc: ssrc,
                            media_ssrc: ext_packet.packet.header.ssrc,
                        })])
                        .await?;
                    return Ok(());
                }

                if re_sync && simulcast.l_ts_calc != 0 {
                    simulcast.l_ts_calc = ext_packet.arrival;
                }

                if simulcast.temporal_supported {
                    let mime = self.down_track_local.mime.lock().await.clone();
                    if mime == *"video/vp8" {
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
                let tdiff = (ext_packet.arrival - simulcast.l_ts_calc) as f64 / 1e6;
                let mut td = (tdiff as u32 * 90) / 1000;
                if td == 0 {
                    td = 1;
                }
                let mut ts_offset = self.ts_offset.lock().await;
                *ts_offset = ext_packet.packet.header.timestamp - (*self.last_ts.lock().await + td);
                let mut sn_offset = self.sn_offset.lock().await;
                *sn_offset =
                    ext_packet.packet.header.sequence_number - *self.last_sn.lock().await - 1;
            } else if simulcast.l_ts_calc == 0 {
                let mut last_ts = self.last_ts.lock().await;
                *last_ts = ext_packet.packet.header.timestamp;
                let mut last_sn = self.last_sn.lock().await;
                *last_sn = ext_packet.packet.header.sequence_number;
                let mime = self.down_track_local.mime.lock().await.clone();
                if mime == *"video/vp8" {
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
            let mime = self.down_track_local.mime.lock().await.clone();
            if mime == *"video/vp8" {
                let (_a, _b, _c, _d) = set_vp8_temporal_layer(ext_packet.clone(), self).await;
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
            simulcast.l_ts_calc = ext_packet.arrival;
        }

        let mut cur_packet = ext_packet.clone();

        let hdr = &mut cur_packet.packet.header;
        hdr.sequence_number = new_sn;
        hdr.timestamp = new_ts;
        hdr.ssrc = ssrc;
        hdr.payload_type = *self.down_track_local.payload_type.lock().await;

        let write_stream_val = self.down_track_local.write_stream.lock().await;
        if let Some(write_stream) = &*write_stream_val {
            trace!(
                "Sending packet to write stream (simulcast) [layer {layer}] {:?}",
                &cur_packet
            );
            write_stream.write_rtp(&cur_packet.packet).await?;
        } else {
            error!(
                "[Track {}] No write stream. Packet will be not transmited.",
                self.id()
            );
        }

        Ok(())
    }
}

impl PartialEq for DownTrack {
    fn eq(&self, other: &Self) -> bool {
        (self.peer_id == other.peer_id) && (self.down_track_local == other.down_track_local)
    }
}

use std::fmt::{Debug, Formatter};
impl Debug for DownTrack {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DownTrack")
            .field("id", &self.id())
            .field("peer_id", &self.peer_id)
            .field("track_type", &self.track_type)
            .field("current_spatial_layer", &self.current_spatial_layer)
            .field("target_spatial_layer", &self.target_spatial_layer)
            .field("temporal_layer", &self.temporal_layer)
            .field("max_spatial_layer", &self.max_spatial_layer)
            .field("max_temporal_layer", &self.max_temporal_layer)
            .finish()
    }
}

#[async_trait]
impl TrackLocal for DownTrack {
    async fn bind(&self, t: &TrackLocalContext) -> RTCResult<RTCRtpCodecParameters> {
        info!("[Track {}] Track bind", self.id());
        self.down_track_local.bind(t).await
    }

    async fn unbind(&self, t: &TrackLocalContext) -> RTCResult<()> {
        info!("[Track {}] Track bind", self.id());
        self.down_track_local.unbind(t).await
    }

    fn id(&self) -> &str {
        self.down_track_local.id()
    }

    fn rid(&self) -> Option<&str> {
        self.down_track_local.rid()
    }

    fn stream_id(&self) -> &str {
        self.down_track_local.stream_id()
    }

    fn kind(&self) -> RTPCodecType {
        self.down_track_local.kind()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
