use std::any::Any;
use std::future::Future;
use std::ops::{Deref, DerefMut};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc::{self, Sender};
use tokio::sync::{watch, Mutex, RwLock};
use tokio::time::Instant;
use webrtc::rtcp::packet::Packet as RtcpPacket;

use webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use webrtc::rtp::packet::Packet as RTP;
use webrtc::rtp_transceiver::rtp_codec::{RTCRtpCodecParameters, RTPCodecType};
use webrtc::rtp_transceiver::rtp_receiver::RTCRtpReceiver;
use webrtc::track::track_remote::TrackRemote;
use webrtc::util::Unmarshal;

use crate::packet::rtcp::RTCPForwarder;
use crate::packet::{AtomicBuffer, BufferIO, VP8};
use crate::track::sequencer::AtomicSequencer;

use super::downtrack::{DownTrack, DownTrackType};
use super::error::{Error, Result};
use super::sequencer::PacketMeta;
use super::{modify_vp8_temporal_payload, simulcast};

pub type RtcpDataReceiver = mpsc::Receiver<Vec<Box<dyn RtcpPacket + Send + Sync>>>;
pub type RtcpDataSender = mpsc::Sender<Vec<Box<dyn RtcpPacket + Send + Sync>>>;

pub type OnCloseHandlerFn =
    Box<dyn (Fn() -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>) + Send + Sync>;

pub struct ReceiverLayer {
    active_sender: watch::Sender<bool>,
    active_rx: Mutex<watch::Receiver<bool>>,
    disposed_tracks: Mutex<Vec<Arc<DownTrack>>>,
    down_tracks: Mutex<Vec<Arc<DownTrack>>>,
    //ingestion_started: AtomicBool,
    rtcp_reader: Arc<RTCPForwarder>,
    rtp_reader: Arc<AtomicBuffer>,
    spatial: AtomicU8,
    up_track: Arc<TrackRemote>,
}

impl ReceiverLayer {
    /// Moves every down track into disposed tracks.
    async fn dispose_all_down_tracks(&self) {
        let mut down_tracks = self.down_tracks.lock().await;
        let mut disposed = self.disposed_tracks.lock().await;

        // Prevents panicking
        if let Err(err) = disposed.try_reserve(down_tracks.len()) {
            error!("dispose_layer Layer {}, err={}", self.spatial_layer(), err);
            return;
        }
        disposed.append(down_tracks.deref_mut());
    }

    pub async fn is_active(&self) -> bool {
        let mut rx = self.active_rx.lock().await;
        *rx.borrow_and_update()
    }

    fn new(up_track: Arc<TrackRemote>, rtp_reader: Arc<AtomicBuffer>, rtcp_reader: Arc<RTCPForwarder>, spatial_layer: u8) -> Arc<Self> {
        let (sender, rx) = watch::channel(false);
        Arc::new(Self {
            active_sender: sender,
            active_rx: rx.into(),
            disposed_tracks: Default::default(),
            down_tracks: Default::default(),
            //ingestion_started: Default::default(),
            rtcp_reader,
            rtp_reader,
            spatial: spatial_layer.into(),
            up_track,
        })
    }

    pub fn run_ingestion(&self, receiver: Arc<RTCRtpReceiver>) {
        self.active_sender.send_replace(true);

        let mut rx_1 = self.active_sender.subscribe();
        let layer = self.spatial_layer();
        let buffer = self.rtp_reader.clone();
        let track = self.up_track.clone();
        
        tokio::spawn(async move {
            if !buffer.bound() {
                warn!("Task run_ingestion failed: RTP reader is not bound.");
                return;
            }

            loop {
                tokio::select! {
                    result = track.read_rtp() => {
                        match result {
                            Ok((pkt, _)) => {
                                buffer.write(pkt).await;
                            },
                            Err(err) => {
                                debug!("Error reading RTP packet: {err}. Exit loop.");
                                break;
                            }
                        }
                    },
                    _ = rx_1.changed() => {
                        if *rx_1.borrow_and_update() {
                            debug!("Layer {layer} has changed to active.");
                        } else {
                            debug!("Layer {layer} has changed to inactive. Exit loop.");
                            break;
                        }
                    }
                }
            }
            let _ = buffer.close().await;
        });
        
        let mut rx_2 = self.active_sender.subscribe();
        let reader = self.rtcp_reader.clone();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    result = receiver.read_rtcp() => {
                        match result {
                            Ok((pkts, _)) => {
                                reader.send_packets(pkts).await;
                            },
                            Err(err) => {
                                debug!("Error reading RTP packet: {err}. Exit loop.");
                                break;
                            },
                        }
                    },
                    _ = rx_2.changed() => {
                        if *rx_2.borrow_and_update() {
                            debug!("Layer {layer} has changed to active.");
                        } else {
                            debug!("Layer {layer} has changed to inactive. Exit loop.");
                            break;
                        }
                    }
                }
            }
        });
    }
    
    async fn remove_down_track(&self, id: &str, close_if_removed: bool) {
        let mut down_tracks = self.down_tracks.lock().await;
        if let Some(idx) = down_tracks.iter().position(|dt| dt.id() == id) {
            let dt = down_tracks.swap_remove(idx);

            if down_tracks.is_empty() {
                self.active_sender.send_replace(false);
            }
            
            drop(down_tracks);
            
            if close_if_removed {
                dt.close().await;
            }
        }
    }
    
    fn spatial_layer(&self) -> u8 {
        self.spatial.load(Ordering::Acquire)
    }
}

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
    /// - `Some(layer)` where `layer` is the layer assigned for the uptrack.
    async fn add_up_track(
        &self,
        track: Arc<TrackRemote>,
        rtp_reader: Arc<AtomicBuffer>,
        rtcp_reader: Arc<RTCPForwarder>,
        best_quality_first: bool,
    ) -> Option<Arc<ReceiverLayer>>;

    /// Stores a [DownTrack] in this receiver.
    ///
    /// # Arguments
    /// - `track`: [DownTrack] to store.
    /// - `layer`: Where the downtrack will be stored. The layer should be provided by [Self::get_available_layer]
    async fn add_down_track(&self, track: Arc<DownTrack>, layer: usize) -> Result<()>;

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

    async fn handle_rtcp(
        &self,
        pkts: Vec<Box<dyn RtcpPacket + Send + Sync>>,
        last_ssrc: u32,
        ssrc: u32,
        sequencer: &AtomicSequencer,
    );

    /// Retransmits all given packets into a given [DownTrack].
    /// # Arguments
    /// - `track`: [DownTrack] used to retransmit packets.
    /// - `packets`: Packets to retransmit.
    async fn retransmit_packets(&self, track: Arc<DownTrack>, packets: &[PacketMeta])
    -> Result<()>;

    /// Deletes and closes a [DownTrack] from a given layer.
    /// # Arguments
    /// - `layer`: Layer of the down track.
    /// - `id`: ID of the down track.
    async fn delete_down_track(&self, layer: usize, id: &str) -> Result<()>;

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

    /// Awaits for an incoming packet from a given layer and forwards them to down tracks.
    ///
    /// # Arguments
    /// - `layer`: Layer to listen.
    ///
    /// # Returns
    /// A `Result<()>` indicating success or failure.
    async fn forward_rtp_packet(&self, layer: usize) -> Result<()>;
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
    last_pli: Mutex<Option<Instant>>,
    #[allow(dead_code)]
    stream: String,
    pub receiver: Arc<RTCRtpReceiver>,
    codec: RTCRtpCodecParameters,
    rtcp_sender: Arc<RtcpDataSender>,
    //buffers: Arc<Mutex<[Option<Arc<AtomicBuffer>>; 3]>>,
    //up_tracks: Arc<Mutex<[Option<Arc<TrackRemote>>; 3]>>,
    //available: Arc<Mutex<[AtomicBool; 3]>>,
    //down_tracks: [Arc<Mutex<Vec<Arc<DownTrack>>>>; 3],
    //pending: [AtomicBool; 3],
    //pending_tracks: [Arc<Mutex<Vec<Arc<DownTrack>>>>; 3],
    // Muta al agregar uptrack, el layer deberia usar arc + mutex interno para conservar estado
    layers: Arc<RwLock<[Option<Arc<ReceiverLayer>>; 3]>>,
    is_simulcast: bool,
    on_close_handler: Arc<Mutex<Option<OnCloseHandlerFn>>>,
}

impl WebRTCReceiver {
    pub fn new(receiver: Arc<RTCRtpReceiver>, track: Arc<TrackRemote>, pid: String) -> Self {
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
            last_pli: Default::default(),
            stream: String::default(),
            rtcp_sender: Arc::new(s),
            //buffers: Arc::default(),
            //up_tracks: Arc::default(),
            //available: Arc::default(),
            //down_tracks: Default::default(),
            //pending: Default::default(),
            //pending_tracks: Default::default(),
            layers: Default::default(),
            on_close_handler: Arc::default(), // ..Default::default()
        }
    }

    pub(super) async fn is_recent_pli(&self, threshold: Duration) -> bool {
        let mut last_pli_sent = self.last_pli.lock().await;

        let now = Instant::now();

        match last_pli_sent.as_ref() {
            Some(last) => {
                if now.duration_since(*last) < threshold {
                    true
                } else {
                    *last_pli_sent = Some(now);
                    false
                }
            }
            None => {
                *last_pli_sent = Some(now);
                false
            }
        }
    }

    pub(crate) async fn layer(&self, layer: usize) -> Option<Arc<ReceiverLayer>> {
        match self.layers.read().await.get(layer) {
            Some(existing) => existing.clone(),
            None => None,
        }
    }

    // Note: layer_mut method removed as it cannot return a mutable reference
    // that outlives the RwLock guard. Use direct access to self.layers.write().await instead.
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
            if let Some(layer_data) = &self.layer(layer).await {
                return layer_data.up_track.ssrc();
            }
        }

        0
    }

    async fn add_up_track(
        &self,
        track: Arc<TrackRemote>,
        rtp_reader: Arc<AtomicBuffer>,
        rtcp_reader: Arc<RTCPForwarder>,
        best_quality_first: bool,
    ) -> Option<Arc<ReceiverLayer>> {
        if self.closed.load(Ordering::Acquire) {
            return None;
        }

        let layer: usize = match track.rid() {
            simulcast::FULL_RESOLUTION => 2,
            simulcast::HALF_RESOLUTION => 1,
            simulcast::QUARTER_RESOLUTION => 0,
            _ => 0,
        };

        let mut layers = self.layers.write().await;

        let layer_data = ReceiverLayer::new(track, rtp_reader, rtcp_reader, layer as u8);
        
        // Create or update the layer
        layers[layer] = Some(layer_data.clone());

        let layers_clone = self.layers.clone();
        let sub_best_quality = |target_layer| async move {
            let layers = layers_clone.read().await;
            for i in 0..target_layer {
                if let Some(Some(layer_data)) = layers.get(i) {
                    for d in layer_data.down_tracks.lock().await.deref() {
                        if let Err(err) =
                            d.set_target_spatial_layer(target_layer as u8, false).await
                        {
                            error!("switch_spatial_layer err: {}", err);
                        }
                    }

                    layer_data.dispose_all_down_tracks().await;
                }
            }
        };

        let layers_clone_2 = self.layers.clone();
        let sub_lowest_quality = |target_layer: usize| async move {
            let layers = layers_clone_2.read().await;
            for l in (target_layer + 1..3).rev() {
                if let Some(Some(layer_data)) = layers.get(l) {
                    for d in layer_data.down_tracks.lock().await.deref() {
                        if let Err(err) =
                            d.set_target_spatial_layer(target_layer as u8, false).await
                        {
                            error!("switch_spatial_layer err: {}", err);
                        }
                    }

                    layer_data.dispose_all_down_tracks().await;
                }
            }
        };

        if self.is_simulcast {
            let layers = self.layers.read().await;
            let layer_2_available = layers[2].is_some();
            let layer_0_available = layers[0].is_some();

            drop(layers);
            if best_quality_first && (layer_2_available || layer == 2) {
                sub_best_quality(layer).await;
            } else if !best_quality_first && (layer_0_available || layer == 0) {
                sub_lowest_quality(layer).await;
            }
        }

        Some(layer_data)
    }

    async fn add_down_track(&self, track: Arc<DownTrack>, layer: usize) -> Result<()> {
        if self.closed.load(Ordering::Relaxed) {
            return Err(Error::ReceiverClosed);
        }

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
                warn!(
                    "Track {track_id} already subscribed. If you wish to replace it, please delete the down track with same ID from layer #0"
                );
                return Err(Error::DuplicatedTrack(layer));
            }

            track.set_initial_layers(0, 0);
            track.set_track_type(DownTrackType::SimpleDownTrack).await;
            info!(
                "WebRTCReceiver::add_down_track Add simple track {}",
                track_id
            );
        }

        match self.store_down_track(layer, track).await {
            Ok(len) => {
                debug!(
                    "[Receiver {}] Track stored. Layer {layer} contains {len} tracks.",
                    self.peer_id
                );
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    async fn handle_rtcp(
        &self,
        pkts: Vec<Box<dyn RtcpPacket + Send + Sync>>,
        last_ssrc: u32,
        ssrc: u32,
        _: &AtomicSequencer,
    ) {
        use webrtc::rtcp::payload_feedbacks::full_intra_request::FullIntraRequest;
        use webrtc::rtcp::payload_feedbacks::receiver_estimated_maximum_bitrate::ReceiverEstimatedMaximumBitrate;
        use webrtc::rtcp::receiver_report::ReceiverReport;
        use webrtc::rtcp::transport_feedbacks::transport_layer_nack::TransportLayerNack;

        let mut fwd_pkts: Vec<Box<dyn RtcpPacket + Send + Sync>> = Vec::new();
        let mut pli_once = true;
        let mut fir_once = true;

        let mut max_rate_packet_loss: u8 = 0;
        let mut expected_min_bitrate: u64 = 0;

        if last_ssrc == 0 {
            return;
        }

        for pkt in &pkts {
            if let Some(picture_loss_indication) =
                pkt.as_any().downcast_ref::<PictureLossIndication>()
            {
                if pli_once {
                    let mut pli = picture_loss_indication.clone();
                    pli.media_ssrc = last_ssrc;
                    pli.sender_ssrc = ssrc;

                    fwd_pkts.push(Box::new(pli));
                    pli_once = false;
                }
            } else if let Some(full_intra_request) = pkt.as_any().downcast_ref::<FullIntraRequest>()
            {
                if fir_once {
                    let mut fir = full_intra_request.clone();
                    fir.media_ssrc = last_ssrc;
                    fir.sender_ssrc = ssrc;

                    fwd_pkts.push(Box::new(fir));
                    fir_once = false;
                }
            } else if let Some(receiver_estimated_max_bitrate) =
                pkt.as_any()
                    .downcast_ref::<ReceiverEstimatedMaximumBitrate>()
            {
                if expected_min_bitrate == 0
                    || expected_min_bitrate > receiver_estimated_max_bitrate.bitrate as u64
                {
                    expected_min_bitrate = receiver_estimated_max_bitrate.bitrate as u64;
                }
            } else if let Some(receiver_report) = pkt.as_any().downcast_ref::<ReceiverReport>() {
                for r in &receiver_report.reports {
                    if max_rate_packet_loss == 0 || max_rate_packet_loss < r.fraction_lost {
                        max_rate_packet_loss = r.fraction_lost;
                    }
                }
            } else if let Some(transport_layer_nack) =
                pkt.as_any().downcast_ref::<TransportLayerNack>()
            {
                debug!("webrtc-rs already retransmits packets back to peer connection. NACK packets ignored.");
                debug!(
                    "Packet retransmition disabled. Could not retransmit {} packets.",
                    transport_layer_nack.nacks.len()
                );
                /*
                let mut nacked_packets: Vec<PacketMeta> = Vec::new();
                for pair in &transport_layer_nack.nacks {
                    let seq_numbers = pair.packet_list();
                    let mut pairs = sequencer.get_seq_no_pairs(&seq_numbers[..]).await;
                    nacked_packets.append(&mut pairs);
                }
                */
                //   receiver.retransmit_packets(track, packets)
            }
        }

        if !fwd_pkts.is_empty() {
            if let Err(err) = self.send_rtcp(fwd_pkts).await {
                warn!("send_rtcp err:{}", err);
            }
        }
    }

    async fn get_bitrates(&self) -> Vec<u64> {
        let mut bitrates = Vec::new();
        let layers = self.layers.read().await;
        for layer in layers.iter().flatten() {
            bitrates.push(layer.rtp_reader.bitrate().await);
        }
        bitrates
    }

    async fn get_available_layer(&self, best_quality_first: bool) -> usize {
        let mut layer = 0;
        if self.is_simulcast {
            let layers = self.layers.read().await;
            for (idx, layer_opt) in layers.iter().enumerate() {
                if layer_opt.is_some() {
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
        let layers = self.layers.read().await;

        for layer in layers.iter().flatten() {
            temporal_layers.push(layer.rtp_reader.max_temporal_layer().await);
        }
        temporal_layers
    }

    /// Removes the downtrack with matching `id` on given `layer`.
    /// The downtrack is closed
    async fn delete_down_track(&self, layer: usize, id: &str) -> Result<()> {
        if self.closed.load(Ordering::Relaxed) {
            return Err(Error::ReceiverClosed);
        }

        let Some(layer_data) = self.layer(layer).await else {
            return Err(Error::FullSpatialLayer(layer as u8));
        };

        layer_data.remove_down_track(id, false).await;
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
        if let Some(layer_data) = self.layer(layer).await.as_ref() {
            (rtp_ts, ntp_ts, _) = layer_data.rtp_reader.get_sender_report_data().await;
        }
        (rtp_ts, ntp_ts)
    }

    async fn retransmit_packets(
        &self,
        track: Arc<DownTrack>,
        packets: &[PacketMeta],
    ) -> Result<()> {
        for packet in packets {
            if let Some(layer) = &self.layer(packet.layer as usize).await {
                let mut data = vec![0_u8; u16::MAX.into()];

                if let Ok(size) = layer
                    .rtp_reader
                    .get_packet(&mut data[..], packet.source_seq_no)
                    .await
                {
                    let mut raw_pkt = Bytes::copy_from_slice(&data[..size]);
                    let pkt = RTP::unmarshal(&mut raw_pkt);
                    match pkt {
                        Ok(mut p) => {
                            p.header.sequence_number = packet.target_seq_no;
                            p.header.timestamp = packet.timestamp;
                            p.header.ssrc = track.ssrc();
                            p.header.payload_type = track.payload_type();

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
                warn!(
                    "No buffer found for layer {}. Retransmition skipped.",
                    packet.layer
                );
                break;
            }
        }

        Ok(())
    }
    fn as_any(&self) -> &(dyn Any + Send + Sync) {
        self
    }

    async fn forward_rtp_packet(&self, layer: usize) -> Result<()> {
        if let Some(receiver_layer) = self.layer(layer).await {
            let mut close = receiver_layer.rtp_reader.subscribe_to_close();
            let mut packet_reader = receiver_layer.rtp_reader.subscribe_to_packet();

            tokio::select! {
                read = packet_reader.recv() => {
                    match read {
                        Ok(pkt) => {
                            //trace!("RTP packet received (arrival {})", pkt.arrival.as_secs_f64());
                            if pkt.key_frame {
                                trace!(
                                    "[Receiver {}] Key frame in layer #{}",
                                    self.peer_id, layer
                                );
                                self.remove_disposed_tracks(receiver_layer.clone()).await;
                            }

                            for dt in receiver_layer.down_tracks.lock().await.deref() {
                                if let Err(err) = dt.forward_rtp(&pkt, layer).await {
                                    warn!("Send RTP to down track {} [#{layer}] failed: {err}", dt.id());
                                };
                            }
                        }
                        Err(RecvError::Lagged(_)) => {
                            let sender_ssrc = rand::random::<u32>();
                            let media_ssrc = self.ssrc(layer).await;
                            self.send_pli(sender_ssrc, media_ssrc).await;
                        },
                        _ => {
                            warn!("[Receiver {}] Extended packet receiver closed.", self.peer_id);
                            return Err(Error::ErrChannelSend);
                        },
                    };
                }

                _ = close.recv() => {
                    info!("Clear disposed tracks from layer #{}", layer);
                    self.remove_disposed_tracks(receiver_layer.clone()).await;
                }
            }
        }
        Ok(())
    }
}

impl WebRTCReceiver {
    async fn remove_disposed_tracks(&self, layer: Arc<ReceiverLayer>) {
        let spatial_layer = layer.spatial.load(Ordering::Acquire);
        let disposed_tracks = {
            let mut guard = layer.disposed_tracks.lock().await;
            std::mem::take(&mut *guard)
        };

        if disposed_tracks.is_empty() {
            return;
        }

        // Procesar cada track: borrar de capa anterior, agregar a esta, forzar switch
        for dt in disposed_tracks {
            let current_layer = dt.current_spatial_layer();
            let id = dt.id();

            // Borrar de capa anterior (si es diferente)
            if current_layer != spatial_layer {
                layer.remove_down_track(&id, true).await;
            }

            // Agregar a capa actual
            if let Err(err) = self
                .store_down_track(spatial_layer as usize, dt.clone())
                .await
            {
                error!(
                    "[Receiver {}] Failed to store down track {}: {}",
                    self.peer_id, id, err
                );
            }

            // Forzar que el track ahora emite desde esta capa
            dt.switch_spatial_layer_forced(spatial_layer);
        }
    }

    pub async fn send_pli(&self, sender_ssrc: u32, media_ssrc: u32) {
        if !self.is_recent_pli(Duration::from_millis(500)).await {
            debug!("Send back PLI, sender ssrc {sender_ssrc}, media_ssrc {media_ssrc}");
            let pkts: Vec<Box<dyn RtcpPacket + Send + Sync>> =
                vec![Box::new(PictureLossIndication {
                    sender_ssrc,
                    media_ssrc,
                })];
            if let Err(err) = self.send_rtcp(pkts).await {
                warn!("Send PLI failed: {err}");
            }
        }
    }

    /// Closes all the not available tracks of the receiver, and set the receiver to closed state.
    /// Calls the on_close handler if it is set.
    pub async fn close_tracks(&self) {
        for layer_opt in self.layers.read().await.as_ref() {
            let Some(down_tracks) = layer_opt.as_ref().map(|l| &l.down_tracks) else {
                continue;
            };
            let guard = down_tracks.lock().await;
            if guard.is_empty() {
                continue;
            }

            for dt in guard.deref() {
                dt.close().await;
            }
        }

        self.closed.store(true, Ordering::Relaxed);

        if let Some(close_handler) = &mut *self.on_close_handler.lock().await {
            close_handler().await;
        }
    }

    async fn down_track_subscribed(&self, layer: usize, dt: Arc<DownTrack>) -> bool {
        if let Some(layer) = self.layer(layer).await {
            let down_tracks = layer.down_tracks.lock().await;
            return down_tracks
                .deref()
                .iter()
                .any(|down_track| *down_track == dt);
        }
        true
    }

    async fn store_down_track(&self, layer: usize, dt: Arc<DownTrack>) -> Result<usize> {
        info!("store_down_track , layer: {}", layer);
        match self.layer(layer).await {
            Some(layer) => {
                let mut down_tracks = layer.down_tracks.lock().await;
                down_tracks.push(dt);
                Ok(down_tracks.len())
            }
            None => Err(Error::ReceiverLayerNotAvailable(layer)),
        }
    }
}
