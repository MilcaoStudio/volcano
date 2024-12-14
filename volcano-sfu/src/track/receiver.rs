use std::any::Any;
use std::pin::Pin;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::Mutex;
use webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use webrtc::rtp::packet::Packet as RTCPacket;
use webrtc::rtcp::packet::Packet as RtcpPacket;
use webrtc::error::Error as RTCError;
use webrtc::rtp_transceiver::rtp_codec::{RTCRtpCodecParameters, RTPCodecType};
use webrtc::rtp_transceiver::rtp_receiver::RTCRtpReceiver;
use webrtc::track::track_remote::TrackRemote;
use webrtc::util::Unmarshal;

use crate::buffer::error::BufferError;
use crate::buffer::buffer::{AtomicBuffer, VP8};

use super::downtrack::{DownTrack, DownTrackType};
use super::error::{Error, Result};
use super::sequencer::PacketMeta;
use super::{modify_vp8_temporal_payload, simulcast};

pub type RtcpDataReceiver = UnboundedReceiver<Vec<Box<dyn RtcpPacket + Send + Sync>>>;
pub type RtcpDataSender = UnboundedSender<Vec<Box<dyn RtcpPacket + Send + Sync>>>;

pub type OnCloseHandlerFn =
    Box<dyn (FnMut() -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>) + Send + Sync>;

#[async_trait]
pub trait Receiver: Send + Sync {
    fn track_id(&self) -> String;
    fn stream_id(&self) -> String;
    fn track_rid(&self) -> String;
    fn codec(&self) -> RTCRtpCodecParameters;
    fn kind(&self) -> RTPCodecType;
    async fn ssrc(&self, layer: usize) -> u32;
    fn set_track_meta(&mut self, track_id: String, stream_id: String);
    async fn add_up_track(
        &self,
        track: Arc<TrackRemote>,
        buffer: Arc<AtomicBuffer>,
        best_quality_first: bool,
    ) -> Option<usize>;
    async fn add_down_track(&self, track: Arc<DownTrack>, best_quality_first: bool);
    async fn switch_down_track(&self, track: Arc<DownTrack>, layer: usize) -> Result<()>;
    async fn get_bitrate(&self) -> Vec<u64>;
    async fn get_max_temporal_layer(&self) -> Vec<i32>;
    async fn retransmit_packets(&self, track: Arc<DownTrack>, packets: &[PacketMeta])
        -> Result<()>;
    async fn delete_down_track(&self, layer: usize, id: String);
    async fn register_on_close(&self, f: OnCloseHandlerFn);
    fn send_rtcp(&self, p: Vec<Box<dyn RtcpPacket + Send + Sync>>) -> Result<()>;
    fn set_rtcp_channel(
        &mut self,
        sender: Arc<UnboundedSender<Vec<Box<dyn RtcpPacket + Send + Sync>>>>,
    );
    async fn get_sender_report_time(&self, layer: usize) -> (u32, u64);
    fn as_any(&self) -> &(dyn Any + Send + Sync);
    async fn write_rtp(&self, layer: usize) -> Result<()>;
}

pub struct WebRTCReceiver {
    #[allow(dead_code)]
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
        let (s, _) = tokio::sync::mpsc::unbounded_channel();
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

        return 0;
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
                    if let Err(err) = d.switch_spatial_layer(target_layer as i32, false).await {
                        error!("switch_spatial_layer err: {}", err);
                    }
                }
            }
        };

        let down_tracks_clone_2 = self.down_tracks.clone();
        let sub_lowest_quality = |target_layer: usize| async move {
            for l in (target_layer + 1..3).rev() {
                let mut dts = down_tracks_clone_2[l].lock().await;
                if dts.len() == 0 {
                    continue;
                }
                for d in &mut *dts {
                    if let Err(err) = d.switch_spatial_layer(target_layer as i32, false).await {
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
        
        // tokio::spawn(async move { self.write_rtp(layer) });
    }
    async fn add_down_track(&self, track: Arc<DownTrack>, best_quality_first: bool) {
        if self.closed.load(Ordering::Relaxed) {
            return;
        }
        let mut layer = 0;

        let track_id = track.id();
        if self.is_simulcast {
            for (idx, v) in self.available.lock().await.iter().enumerate() {
                if v.load(Ordering::Relaxed) {
                    layer = idx;
                    if !best_quality_first {
                        break;
                    }
                }
            }
            if self.down_track_subscribed(layer, track.clone()).await {
                debug!("Track {} already subscribed", track_id);
                return;
            }
            track.set_initial_layers(layer as i32, 2);
            track.set_max_spatial_layer(2);
            track.set_max_temporal_layer(2);
            track.set_last_ssrc(self.ssrc(layer).await);
            track
                .set_track_type(DownTrackType::SimulcastDownTrack)
                .await;
            info!("[WebRTCReceiver::add_down_track] Add simulcast track {}", track_id);
        } else {
            if self.down_track_subscribed(layer, track.clone()).await {
                debug!("Track {} already subscribed", track_id);
                return;
            }

            track.set_initial_layers(0, 0);
            track.set_track_type(DownTrackType::SimpleDownTrack).await;
            info!("WebRTCReceiver::add_down_track Add simple track {}", track_id);
        }

        self.store_down_track(layer, track).await
    }
    async fn switch_down_track(&self, track: Arc<DownTrack>, layer: usize) -> Result<()> {
        if self.closed.load(Ordering::Relaxed) {
            error!("Receiver is closed");
            return Err(Error::ErrNoReceiverFound);
        }

        if self.available.lock().await[layer].load(Ordering::Relaxed) {
            info!("[Down track {}] Marked as pending", track.id());
            self.pending[layer].store(true, Ordering::Relaxed);
            self.pending_tracks[layer].lock().await.push(track);
            return Ok(());
        }
        Err(Error::ErrNoReceiverFound)
    }

    async fn get_bitrate(&self) -> Vec<u64> {
        let mut bitrates = Vec::new();
        for buff in (*self.buffers.lock().await).iter().flatten() {
            //if let Some(b) = buff {
            bitrates.push(buff.bitrate().await)
            // }
        }
        bitrates
    }
    async fn get_max_temporal_layer(&self) -> Vec<i32> {
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

    /// Closes and removes the downtrack with matching `id` on given `layer`
    async fn delete_down_track(&self, layer: usize, id: String) {
        if self.closed.load(Ordering::Relaxed) {
            return;
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
    }

    async fn register_on_close(&self, f: OnCloseHandlerFn) {
        let mut handler = self.on_close_handler.lock().await;
        *handler = Some(f);
    }

    fn send_rtcp(&self, p: Vec<Box<dyn RtcpPacket + Send + Sync>>) -> Result<()> {
        // Checks if first packet is PLI
        if let Some(packet) = p.get(0) {
            if packet.as_any().downcast_ref::<webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication>().is_some() {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64;

                let threshold: u64 = 500;

                // Last PLI is recent (<500 milliseconds), do not send rtcp
                if now - self.last_pli.load(Ordering::Relaxed) < threshold {
                    return Ok(());
                }
                // Store current time as last PLI
                self.last_pli.store(now, Ordering::Relaxed);
            }
        }

        if self.rtcp_sender.send(p).is_err() {
            return Err(Error::ErrChannelSend);
        }

        Ok(())
    }

    fn set_rtcp_channel(
        &mut self,
        sender: Arc<UnboundedSender<Vec<Box<dyn RtcpPacket + Send + Sync>>>>,
    ) {
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
                let mut data = vec![0_u8; 65535];

                if let Ok(size) = buffer.get_packet(&mut data[..], packet.source_seq_no).await {
                    let mut raw_data = Vec::new();
                    raw_data.extend_from_slice(&data[..size]);

                    let mut raw_pkt = Bytes::from(raw_data);
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
                        Err(_) => {
                            continue;
                        }
                    }
                }
            } else {
                break;
            }
        }

        Ok(())
    }
    fn as_any(&self) -> &(dyn Any + Send + Sync) {
        self
    }

    async fn write_rtp(&self, layer: usize) -> Result<()> {
        loop {
            if let Some(buffer) = &self.buffers.lock().await[layer] {
                match buffer.read_extended().await {
                    Ok(pkt) => {
                        if self.is_simulcast && self.pending[layer].load(Ordering::Relaxed) {
                            debug!("Reading packet on layer {layer} in simulcast receiver");
                            if pkt.key_frame {
                                //use tmp_val here just to skip the build error
                                let mut pending_tracks = Vec::new();
                                for dt in &*self.pending_tracks[layer].lock().await {
                                    pending_tracks.push((dt.current_spatial_layer() as usize, dt.id().clone(), dt.clone()));
                                }
                                for (dt_layer, id, dt) in pending_tracks {
                                    // Delete downtrack from its layer
                                    self.delete_down_track(dt_layer, id).await;
                                    // Store downtrack in current layer
                                    self.store_down_track(layer, dt.clone()).await;
                                    dt.switch_spatial_layer_forced(layer as i32);
                                }
                                // Cleanup
                                self.pending_tracks[layer].lock().await.clear();
                                self.pending[layer].store(false, Ordering::Relaxed);
                            } else {
                                let sender_ssrc = rand::random::<u32>();
                                let media_ssrc = self.ssrc(layer).await;
                                debug!("Send PLI, sender ssrc {sender_ssrc}, media_ssrc {media_ssrc}");
                                self.send_rtcp(vec![Box::new(PictureLossIndication {
                                    sender_ssrc,
                                    media_ssrc,
                                })])?;
                            }
                        }

                        let mut delete_down_track_params = Vec::new();
                        {
                            let dts = self.down_tracks[layer].lock().await;

                            for dt in &*dts {
                                
                                if let Err(err) = dt.write_rtp(pkt.clone(), layer).await {
                                    if let Error::ErrWebRTC(e) = err {
                                        match e {
                                            RTCError::ErrClosedPipe
                                            | RTCError::ErrDataChannelNotOpen
                                            | RTCError::ErrConnectionClosed => {
                                                error!(
                                                    "down track write error {}, layer {}, queued for remove",
                                                    e,
                                                    layer
                                                );
                                                delete_down_track_params
                                                    .push((layer, dt.id().clone()));
                                            }
                                            _ => {
                                                error!("down track unknown write error {}, layer {}", e, layer);
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        // Delete downtracks which received error at sending RTP
                        for (layer, id) in delete_down_track_params {
                            self.delete_down_track(layer, id).await;
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
    #[allow(dead_code)]
    async fn close_tracks(&self) {
        for (idx, a) in self.available.lock().await.iter().enumerate() {
            if a.load(Ordering::Relaxed) {
                continue;
            }
            let down_tracks = self.down_tracks[idx].lock().await;

            for dt in &*down_tracks {
                dt.close().await;
            }
        }

        if let Some(close_handler) = &mut *self.on_close_handler.lock().await {
            close_handler().await;
        }
    }
    async fn down_track_subscribed(&self, layer: usize, dt: Arc<DownTrack>) -> bool {
        let down_tracks = self.down_tracks[layer].lock().await;
        for down_track in &*down_tracks {
            if **down_track == *dt {
                return true;
            }
        }

        false
    }

    async fn store_down_track(&self, layer: usize, dt: Arc<DownTrack>) {
        info!("store_down_track , layer: {}", layer);
        self.down_tracks[layer].lock().await.push(dt);
    }
}