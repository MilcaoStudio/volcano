use std::any::Any;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use tokio::sync::mpsc::{self, Sender};
use tokio::sync::Mutex;
use webrtc::error::Error as RTCError;
use webrtc::rtcp::packet::Packet as RtcpPacket;
use webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use webrtc::rtp::packet::Packet as RTCPacket;
use webrtc::rtp_transceiver::rtp_codec::{RTCRtpCodecParameters, RTPCodecType};
use webrtc::rtp_transceiver::rtp_receiver::RTCRtpReceiver;
use webrtc::track::track_remote::TrackRemote;
use webrtc::util::Unmarshal;

use crate::buffer::error::BufferError;
use crate::buffer::{AtomicBuffer, VP8};

use super::downtrack::{DownTrack, DownTrackType};
use super::error::{Error, Result};
use super::sequencer::PacketMeta;
use super::{modify_vp8_temporal_payload, simulcast};

pub type RtcpDataReceiver = mpsc::Receiver<Vec<Box<dyn RtcpPacket + Send + Sync>>>;
pub type RtcpDataSender = mpsc::Sender<Vec<Box<dyn RtcpPacket + Send + Sync>>>;

pub type OnCloseHandlerFn =
    Box<dyn (FnMut() -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>) + Send + Sync>;

/// Receiver of a media track in an RTP/RTCP context.
///
/// This trait abstracts common operations such as SSRC management, bitrate estimation,
/// track switching, RTCP reporting, and event handling. It is suitable for use in simulcast,
/// scalable video coding (SVC), or peer-to-peer streaming architectures.
#[async_trait]
pub trait Receiver: Send + Sync {

    /// track_id is the unique identifier for this receiver's tracks.
    fn track_id(&self) -> String;

    /// stream_id is the group this receiver's tracks belongs too. This must be unique.
    fn stream_id(&self) -> String;

    /// RTP Stream ID of this receiver's tracks. In simulcast tracks you will have multiple tracks with the same ID, but different RID values.
    fn track_rid(&self) -> String;

    /// Codec of this receiver's tracks.
    fn codec(&self) -> RTCRtpCodecParameters;

    /// Kind of tracks.
    fn kind(&self) -> RTPCodecType;

    /// SSRC (Synchronization Source) of the track located in a given spatial (quality) layer.
    /// # Arguments
    /// - `layer` - Spatial layer of the track.
    /// # Returns
    /// - SSRC of the track located in the given spatial layer.
    /// - If the layer is out of bounds, returns 0.
    /// - If the track is not bound to the layer, returns 0.
    async fn ssrc(&self, layer: usize) -> u32;

    /// Sets the metadata of the tracks.
    /// # Arguments
    /// - `track_id` - New ID of the tracks.
    /// - `stream_id` - New Stream ID of the tracks.
    fn set_track_meta(&mut self, track_id: String, stream_id: String);

    /// Adds an upstream track to forward packets from.
    ///
    /// # Arguments
    /// - `track`: Reference to the remote track.
    /// - `buffer`: Atomic buffer used for intermediate storage.
    /// - `best_quality_first`: If true and the track is simulcast, returns higher quality layer.
    ///
    /// # Returns
    /// - `None` if the receiver is closed.
    /// - `Some(0)` if the track is not simulcast, or it is simulcast and `best_quality_first` is false.
    /// - `Some(layer)` where `layer` is the layer of the best quality track.
    async fn add_up_track(
        &self,
        track: Arc<TrackRemote>,
        buffer: Arc<AtomicBuffer>,
        best_quality_first: bool,
    ) -> Option<usize>;

    /// Stores a [DownTrack] in this receiver.
    ///
    /// # Arguments
    /// - `track`: [DownTrack] to store.
    /// - `best_quality_first`: Stores the track in the highest quality layer available (only used for simulcast).
    async fn add_down_track(&self, track: Arc<DownTrack>, best_quality_first: bool) -> Result<()>;

    /// Adds a [DownTrack] to the given layer, if the layer is available.
    /// # Arguments
    /// - `track`: [DownTrack] to add.
    /// - `layer`: Layer to add the track to.
    /// # Errors
    /// - `Error::ReceiverLayerNotAvailable` if the layer is not available.
    /// - `Error::ReceiverClosed` if the receiver is closed.
    async fn switch_down_track(&self, track: Arc<DownTrack>, layer: usize) -> Result<()>;

    /// Bitrates of each layer.
    async fn get_bitrates(&self) -> Vec<u64>;

    /// Gets the next available layer for the next track.
    /// 
    /// # Arguments
    /// - `best_quality_first`: If true, returns the highest quality layer available. Otherwise, returns the lowest available.
    /// 
    /// # Returns
    /// - Layer index (0-2) if an available layer is found for a simulcast track.
    /// - Layer 0 for a simple track.
    async fn get_available_layer(&self, best_quality_first: bool) -> usize;

    /// Returns the maximum temporal layer of each layer.
    async fn get_max_temporal_layers(&self) -> Vec<i32>;

    /// Retransmits all given packets into a given [DownTrack].
    /// # Arguments
    /// - `track`: [DownTrack] used to retransmit packets.
    /// - `packets`: Packets to retransmit.
    async fn retransmit_packets(&self, track: Arc<DownTrack>, packets: &[PacketMeta]) -> Result<()>;

    /// Deletes and closes a [DownTrack] from a given layer.
    /// # Arguments
    /// - `layer`: Layer of the down track.
    /// - `id`: ID of the down track.
    async fn delete_down_track(&self, layer: usize, id: String) -> Result<()>;

    /// Registers a function to be called when the receiver is closed.
    async fn register_on_close(&self, f: OnCloseHandlerFn);

    /// Sends RTCP packets to the receiver.
    /// # Arguments
    /// - `p`: RTCP packets to send.
    /// # Errors
    /// - `Error::ErrChannelSend` if the sender channel is closed.
    async fn send_rtcp(&self, p: Vec<Box<dyn RtcpPacket + Send + Sync>>) -> Result<()>;

    /// Sets the RTCP channel for the receiver.
    /// # Arguments
    /// - `sender`: RTCP sender channel.
    fn set_rtcp_channel(&mut self, sender: Arc<Sender<Vec<Box<dyn RtcpPacket + Send + Sync>>>>);

    /// Gets the timestamp of the last Sender Report (SR) for a given layer.
    ///
    /// # Arguments
    /// - `layer`: Target layer.
    ///
    /// # Returns
    /// A tuple `(ntp_timestamp, rtp_timestamp)` of the last SR.
    async fn get_sender_report_time(&self, layer: usize) -> (u32, u64);

    /// Converts the receiver into a `dyn Any` object for dynamic downcasting.
    fn as_any(&self) -> &(dyn Any + Send + Sync);

    /// Awaits for incoming packets from a given layer and forwards them.
    /// 
    /// ## Blocking
    /// It is recommended to use this function inside an async task.
    ///
    /// # Arguments
    /// - `layer`: Target layer.
    ///
    /// # Returns
    /// A `Result<()>` indicating success or failure.
    async fn write_rtp(&self, layer: usize) -> Result<()>;
}

/// A wrapper around an RTCRtpReceiver that provides additional functionality for handling incoming media packets.
/// 
/// It contains 3 layers for simulcast tracks. For simple tracks, a single layer (#0) is used.
/// Each layer is associated with a DownTrack, a buffer, a remote track, and a set of pending tracks.
/// # Examples
/// ```
/// use std::sync::Arc;
/// use webrtc::{api::APIBuilder, error::Result, peer_connection::configuration::RTCConfiguration, rtp_transceiver::{RTCRtpTransceiver, rtp_receiver::RTCRtpReceiver}, track::track_remote::TrackRemote};
/// 
/// use volcano_sfu::track::receiver::WebRTCReceiver;
/// 
/// #[tokio::main]
/// async fn main() -> Result<()> {
///     let api = APIBuilder::new().build();
///     let pc = api.new_peer_connection(RTCConfiguration::default()).await?;
///     pc.on_track(Box::new(move |track: Arc<TrackRemote>, receiver: Arc<RTCRtpReceiver>, _: Arc<RTCRtpTransceiver>| {
///         let receiver = WebRTCReceiver::new(receiver, track, "test".to_owned());
///         Box::pin(async move {})
///     }));
///     Ok(())
/// }
pub struct WebRTCReceiver {
    peer_id: String,
    track_id: String,
    track_rid: String,
    stream_id: String,
    kind: RTPCodecType,
    closed: AtomicBool,
    #[allow(dead_code)]
    bandwidth: u64,
    last_pli: AtomicU64,
    #[allow(dead_code)]
    stream: String,
    pub receiver: Arc<RTCRtpReceiver>,
    codec: RTCRtpCodecParameters,
    rtcp_sender: Arc<RtcpDataSender>,
    buffers: Arc<Mutex<[Option<Arc<AtomicBuffer>>; 3]>>,
    up_tracks: Arc<Mutex<[Option<Arc<TrackRemote>>; 3]>>,
    available: Arc<Mutex<[AtomicBool; 3]>>,
    down_tracks: [Arc<Mutex<Vec<Arc<DownTrack>>>>; 3],
    pending: [AtomicBool; 3],
    pending_tracks: [Arc<Mutex<Vec<Arc<DownTrack>>>>; 3],
    is_simulcast: bool,
    on_close_handler: Arc<Mutex<Option<OnCloseHandlerFn>>>,
}

impl WebRTCReceiver {

    pub async fn new(receiver: Arc<RTCRtpReceiver>, track: Arc<TrackRemote>, pid: String) -> Self {
        let (s, _) = tokio::sync::mpsc::channel(1024);
        Self {
            peer_id: pid,
            receiver,
            track_id: track.id(),
            track_rid: track.rid().to_owned(),
            stream_id: track.stream_id(),
            codec: track.codec(),
            kind: track.kind(),
            is_simulcast: !track.rid().is_empty(),
            closed: AtomicBool::default(),
            bandwidth: 0,
            last_pli: AtomicU64::default(),
            stream: String::default(),
            rtcp_sender: Arc::new(s),
            buffers: Arc::default(),
            up_tracks: Arc::default(),
            available: Arc::default(),
            down_tracks: Default::default(),
            pending: Default::default(),
            pending_tracks: Default::default(),
            on_close_handler: Arc::default(), // ..Default::default()
        }
    }

    pub(super) async fn is_recent_pli(&self) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        let threshold: u64 = 500;

        // Last PLI is recent (<500 milliseconds), do not send rtcp
        if now - self.last_pli.load(Ordering::Relaxed) < threshold {
            return true;
        }
        
        // Store current time as last PLI
        self.last_pli.store(now, Ordering::Relaxed);
        false
    }
}
#[async_trait]
impl Receiver for WebRTCReceiver {
    fn set_track_meta(&mut self, track_id: String, stream_id: String) {
        self.stream_id = stream_id;
        self.track_id = track_id;
    }

    fn stream_id(&self) -> String {
        self.stream_id.clone()
    }
    fn track_id(&self) -> String {
        self.track_id.clone()
    }
    fn track_rid(&self) -> String {
        self.track_rid.clone()
    }
    fn codec(&self) -> RTCRtpCodecParameters {
        self.codec.clone()
    }
    fn kind(&self) -> RTPCodecType {
        self.kind
    }
    async fn ssrc(&self, layer: usize) -> u32 {
        if layer < 3 {
            if let Some(track) = &self.up_tracks.lock().await[layer] {
                return track.ssrc();
            }
        }

        0
    }

    async fn add_up_track(
        &self,
        track: Arc<TrackRemote>,
        buffer: Arc<AtomicBuffer>,
        best_quality_first: bool,
    ) -> Option<usize> {
        if self.closed.load(Ordering::Acquire) {
            return None;
        }

        let layer: usize = match track.rid() {
            simulcast::FULL_RESOLUTION => 2,
            simulcast::HALF_RESOLUTION => 1,
            simulcast::QUARTER_RESOLUTION => 0,
            _ => 0,
        };

        let up_tracks = &mut self.up_tracks.lock().await;
        up_tracks[layer] = Some(track);
        let buffers = &mut self.buffers.lock().await;
        buffers[layer] = Some(buffer);
        let available = &mut self.available.lock().await;
        available[layer] = AtomicBool::new(true);

        let down_tracks_clone = self.down_tracks.clone();
        let sub_best_quality = |target_layer| async move {
            for down_tracks in down_tracks_clone.iter().take(target_layer) {
                let mut down_tracks_val = down_tracks.lock().await;
                if down_tracks_val.is_empty() {
                    continue;
                }
                for d in &mut *down_tracks_val {
                    if let Err(err) = d.switch_spatial_layer(target_layer as u8, false).await {
                        error!("switch_spatial_layer err: {}", err);
                    }
                }
            }
        };

        let down_tracks_clone_2 = self.down_tracks.clone();
        let sub_lowest_quality = |target_layer: usize| async move {
            for l in (target_layer + 1..3).rev() {
                let mut dts = down_tracks_clone_2[l].lock().await;
                if dts.is_empty() {
                    continue;
                }
                for d in &mut *dts {
                    if let Err(err) = d.switch_spatial_layer(target_layer as u8, false).await {
                        error!("switch_spatial_layer err: {}", err);
                    }
                }
            }
        };

        if self.is_simulcast {
            if best_quality_first
                && (self.available.lock().await[2].load(Ordering::Relaxed) || layer == 2)
            {
                sub_best_quality(layer).await;
            } else if !best_quality_first
                && (self.available.lock().await[0].load(Ordering::Relaxed) || layer == 0)
            {
                sub_lowest_quality(layer).await;
            }
        }

        Some(layer)
    }

    async fn add_down_track(&self, track: Arc<DownTrack>, best_quality_first: bool) -> Result<()> {
        if self.closed.load(Ordering::Relaxed) {
            return Err(Error::ReceiverClosed);
        }
        
        let layer = self.get_available_layer(best_quality_first).await;

        let track_id = track.id();
        if self.is_simulcast {
            if self.down_track_subscribed(layer, track.clone()).await {
                debug!("Track {} already subscribed", track_id);
                return Err(Error::DuplicatedTrack(layer));
            }
            track.set_initial_layers(layer as u8, 2);
            track.set_max_spatial_layer(2);
            track.set_max_temporal_layer(2);
            track.set_last_ssrc(self.ssrc(layer).await);
            track
                .set_track_type(DownTrackType::SimulcastDownTrack)
                .await;
            info!(
                "[WebRTCReceiver::add_down_track] Add simulcast track {}",
                track_id
            );
        } else {
            if self.down_track_subscribed(layer, track.clone()).await {
                warn!("Track {track_id} already subscribed. If you wish to replace it, please delete the down track with same ID from layer #0");
                return Err(Error::DuplicatedTrack(layer));
            }

            track.set_initial_layers(0, 0);
            track.set_track_type(DownTrackType::SimpleDownTrack).await;
            info!(
                "WebRTCReceiver::add_down_track Add simple track {}",
                track_id
            );
        }

        self.store_down_track(layer, track).await;
        Ok(())
    }

    async fn switch_down_track(&self, track: Arc<DownTrack>, layer: usize) -> Result<()> {
        if self.closed.load(Ordering::Relaxed) {
            return Err(Error::ReceiverClosed);
        }

        if self.available.lock().await[layer].load(Ordering::Relaxed) {
            info!("[Down track {}] Marked as pending", track.id());
            self.pending[layer].store(true, Ordering::Relaxed);
            self.pending_tracks[layer].lock().await.push(track);
            return Ok(());
        }
        
        Err(Error::ReceiverLayerNotAvailable(layer))
    }

    async fn get_bitrates(&self) -> Vec<u64> {
        let mut bitrates = Vec::new();
        for buff in (*self.buffers.lock().await).iter().flatten() {
            bitrates.push(buff.bitrate().await)
        }
        bitrates
    }

    async fn get_available_layer(&self, best_quality_first: bool) -> usize {
        let mut layer = 0;
        if self.is_simulcast {
            for (idx, v) in self.available.lock().await.iter().enumerate() {
                if v.load(Ordering::Relaxed) {
                    layer = idx;
                    if !best_quality_first {
                        return layer;
                    }
                }
            }
        }

        layer
    }

    async fn get_max_temporal_layers(&self) -> Vec<i32> {
        let mut temporal_layers = Vec::new();

        for (idx, a) in self.available.lock().await.iter().enumerate() {
            if a.load(Ordering::Relaxed) {
                if let Some(buff) = &self.buffers.lock().await[idx] {
                    temporal_layers.push(buff.max_temporal_layer().await)
                }
            }
        }
        temporal_layers
    }

    /// Removes the downtrack with matching `id` on given `layer`.
    /// The downtrack is closed, but the internal track may still be open
    async fn delete_down_track(&self, layer: usize, id: String) -> Result<()> {
        if self.closed.load(Ordering::Relaxed) {
            return Err(Error::ReceiverClosed);
        }

        let mut down_tracks = self.down_tracks[layer].lock().await;
        let mut idx: usize = 0;
        for dt in &*down_tracks {
            if dt.id() == id {
                dt.close().await;
                break;
            }
            idx += 1;
        }

        down_tracks.swap_remove(idx);
        Ok(())
    }

    async fn register_on_close(&self, f: OnCloseHandlerFn) {
        let mut handler = self.on_close_handler.lock().await;
        *handler = Some(f);
    }

    async fn send_rtcp(&self, p: Vec<Box<dyn RtcpPacket + Send + Sync>>) -> Result<()> {
        if self.rtcp_sender.send(p).await.is_err() {
            return Err(Error::ErrChannelSend);
        }

        Ok(())
    }

    fn set_rtcp_channel(&mut self, sender: Arc<Sender<Vec<Box<dyn RtcpPacket + Send + Sync>>>>) {
        self.rtcp_sender = sender;
    }

    async fn get_sender_report_time(&self, layer: usize) -> (u32, u64) {
        let mut rtp_ts = 0;
        let mut ntp_ts = 0;
        if let Some(buffer) = &self.buffers.lock().await[layer] {
            (rtp_ts, ntp_ts, _) = buffer.get_sender_report_data().await;
        }
        (rtp_ts, ntp_ts)
    }

    async fn retransmit_packets(
        &self,
        track: Arc<DownTrack>,
        packets: &[PacketMeta],
    ) -> Result<()> {
        for packet in packets {
            if let Some(buffer) = &self.buffers.lock().await[packet.layer as usize] {
                let mut data = vec![0_u8; u16::MAX.into()];

                if let Ok(size) = buffer.get_packet(&mut data[..], packet.source_seq_no).await {
                    let mut raw_pkt = Bytes::copy_from_slice(&data[..size]);
                    let pkt = RTCPacket::unmarshal(&mut raw_pkt);
                    match pkt {
                        Ok(mut p) => {
                            p.header.sequence_number = packet.target_seq_no;
                            p.header.timestamp = packet.timestamp;
                            p.header.ssrc = track.ssrc().await;
                            p.header.payload_type = track.payload_type().await;

                            let mut payload = BytesMut::new();
                            payload.extend_from_slice(&p.payload.slice(..));

                            if track.simulcast.lock().await.temporal_supported {
                                let mime = track.mime().await;
                                if mime.as_str() == "video/vp8" {
                                    let mut vp8 = VP8::default();
                                    if vp8.unmarshal(&p.payload).is_err() {
                                        continue;
                                    }
                                    let (tlz0_id, pic_id) = packet.get_vp8_payload_meta();
                                    modify_vp8_temporal_payload(
                                        &mut payload,
                                        vp8.picture_id_idx as usize,
                                        vp8.tlz_idx as usize,
                                        pic_id,
                                        tlz0_id,
                                        vp8.mbit,
                                    )
                                }
                            }

                            if track.write_raw_rtp(p).await.is_ok() {
                                track.update_stats(size as u32);
                            }
                        }
                        Err(err) => {
                            warn!("Invalid RTC packet: {err}. Skipped.");
                            continue;
                        }
                    }
                }
            } else {
                warn!("No buffer found for layer {}. Retransmition skipped.", packet.layer);
                break;
            }
        }

        Ok(())
    }
    fn as_any(&self) -> &(dyn Any + Send + Sync) {
        self
    }

    async fn write_rtp(&self, layer: usize) -> Result<()> {
        info!("[Receiver {}] Write ExtPacket for layer {layer} started.", self.track_id);
        let mut interval = tokio::time::interval(Duration::from_micros(125));
        loop {
            if let Some(buffer) = &self.buffers.lock().await[layer] {
                interval.tick().await;
                match buffer.read_extended().await {
                    Ok(pkt) => {
                        if self.is_simulcast && self.pending[layer].load(Ordering::Relaxed) {
                            debug!("Reading packet on layer {layer} in simulcast receiver");
                            if pkt.key_frame {
                                //use tmp_val here just to skip the build error
                                let mut pending_tracks = Vec::new();
                                for dt in &*self.pending_tracks[layer].lock().await {
                                    pending_tracks.push((
                                        dt.current_spatial_layer() as usize,
                                        dt.id().clone(),
                                        dt.clone(),
                                    ));
                                }
                                for (dt_layer, id, dt) in pending_tracks {
                                    // Delete downtrack from its layer
                                    if let Err(err) = self.delete_down_track(dt_layer, id).await {
                                        error!("[Receiver {}] Failed to delete down track: {}", self.peer_id, err);
                                    };
                                    // Store downtrack in current layer
                                    self.store_down_track(layer, dt.clone()).await;
                                    dt.switch_spatial_layer_forced(layer as u8);
                                }
                                // Cleanup
                                self.pending_tracks[layer].lock().await.clear();
                                self.pending[layer].store(false, Ordering::Relaxed);
                            } else {
                                if !self.is_recent_pli().await {
                                    let sender_ssrc = rand::random::<u32>();
                                    let media_ssrc = self.ssrc(layer).await;
                                    
                                    
                                    debug!(
                                        "Send PLI, sender ssrc {sender_ssrc}, media_ssrc {media_ssrc}"
                                    );
                                    self.send_rtcp(vec![Box::new(PictureLossIndication {
                                        sender_ssrc,
                                        media_ssrc,
                                    })])
                                    .await?;
                                }
                            }
                        }

                        let mut delete_down_track_params = Vec::new();
                        {
                            let dts = self.down_tracks[layer].lock().await;
                            
                            for dt in &*dts {
                                if let Err(Error::ErrWebRTC(e)) = dt.forward_rtp(pkt.clone(), layer).await
                                {
                                    match e {
                                        RTCError::ErrClosedPipe
                                        | RTCError::ErrDataChannelNotOpen
                                        | RTCError::ErrConnectionClosed => {
                                            error!(
                                                    "down track write error {}, layer {}, queued for remove",
                                                    e,
                                                    layer
                                                );
                                            delete_down_track_params.push((layer, dt.id().clone()));
                                        }
                                        _ => {
                                            error!(
                                                "down track unknown write error {}, layer {}",
                                                e, layer
                                            );
                                        }
                                    }
                                }
                            }
                        }

                        // Delete downtracks which received error at sending RTP
                        for (layer, id) in delete_down_track_params {
                            if let Err(err) = self.delete_down_track(layer, id).await {
                                error!("[Receiver {}] Failed to delete down track: {}", self.peer_id, err);
                            };
                        }
                    }
                    Err(e) => match e {
                        BufferError::ErrIOEof => {
                            error!("read_extended -> Buffer EOF");
                        }
                        _ => {
                            error!("read_extended -> {e}");
                        }
                    },
                }
            }
        }
    }
}

impl WebRTCReceiver {
    
    /// Closes all the not available tracks of the receiver, and set the receiver to closed state.
    /// Calls the on_close handler if it is set.
    pub async fn close_tracks(&self) {
        for dt in self.down_tracks.iter() {
            let down_tracks = dt.lock().await;
            if down_tracks.is_empty() {
                continue;
            }
            
            for dt in &*down_tracks {
                dt.close().await;
            }
        }

        self.closed.store(true, Ordering::Relaxed);

        if let Some(close_handler) = &mut *self.on_close_handler.lock().await {
            close_handler().await;
        }
    }
    
    async fn down_track_subscribed(&self, layer: usize, dt: Arc<DownTrack>) -> bool {
        let down_tracks = self.down_tracks[layer].lock().await;
        down_tracks.iter().any(|down_track| *down_track == dt)
    }

    async fn store_down_track(&self, layer: usize, dt: Arc<DownTrack>) {
        info!("store_down_track , layer: {}", layer);
        self.down_tracks[layer].lock().await.push(dt);
    }
}