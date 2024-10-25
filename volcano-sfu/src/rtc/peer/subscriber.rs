use std::pin::Pin;
use std::future::Future;
use std::{collections::HashMap, sync::Arc};

use tokio::sync::Mutex;
use tokio::time::{sleep, Duration};
use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
use webrtc::ice_transport::ice_connection_state::RTCIceConnectionState;
use webrtc::ice_transport::ice_gatherer::OnLocalCandidateHdlrFn;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtcp::source_description::SourceDescription;
use webrtc::rtp_transceiver::rtp_codec::RTPCodecType;
use webrtc::track::track_local::TrackLocal;
use webrtc::{
    api::media_engine::MediaEngine, data_channel::RTCDataChannel,
    ice_transport::ice_candidate::RTCIceCandidateInit, peer_connection::RTCPeerConnection,
};

use super::api;
use crate::rtc::config::WebRTCTransportConfig;
use crate::rtc::message::RemoteMedia;
use crate::track::downtrack::DownTrack;
use crate::track::error::Result;

const HIGH_VALUE: &str = "high";
const MEDIA_VALUE: &str = "medium";
const LOW_VALUE: &str = "low";
const MUTED_VALUE: &str = "none";
pub const API_CHANNEL_LABEL: &str = "System";

pub struct Subscriber {
    pub id: String,
    pub pc: Arc<RTCPeerConnection>,
    pub m: Arc<Mutex<MediaEngine>>,

    tracks: Arc<Mutex<HashMap<String, Vec<Arc<DownTrack>>>>>,
    channels: Arc<Mutex<HashMap<String, Arc<RTCDataChannel>>>>,
    candidates: Arc<Mutex<Vec<RTCIceCandidateInit>>>,
    on_negotiate: Arc<Mutex<Option<OnNegotiateFn>>>,
    pub no_auto_subscribe: bool,
}

pub type OnNegotiateFn =
    Box<dyn (FnMut() -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'static>>) + Send + Sync>;

impl Subscriber {
    pub async fn new(id: String, c: Arc<WebRTCTransportConfig>) -> Result<Self> {
        let pc = api::create_subscriber_connection(c).await?;
        let subscriber = Subscriber {
            id,
            pc,
            m: Default::default(),
            tracks: Default::default(),
            channels: Default::default(),
            candidates: Default::default(),
            on_negotiate: Default::default(),
            no_auto_subscribe: Default::default(),
        };
        subscriber.on_ice_connection_state_change().await;
        Ok(subscriber)
    }

    pub async fn add_data_channel(&self, label: &str) -> Result<()> {
        let ndc = self
            .pc
            .create_data_channel(label, Some(RTCDataChannelInit::default()))
            .await?;
        let tracks_out = self.tracks.clone();

        let ndc_1 = ndc.clone();
        ndc.on_message(Box::new(move |msg| {
            let data = String::from_utf8(msg.data.to_vec())
                .inspect_err(|_| error!("Error parsing message as string"))
                .unwrap();
            info!("[{}] Message received: {data}", ndc_1.label());
            let read_remote_media = serde_json::from_str::<RemoteMedia>(&data);
            let tracks_in = tracks_out.clone();
            
            Box::pin(async move {
                
                match read_remote_media {
                    Ok(remote_media) => {
                        if let Some(tracks) =
                            tracks_in.lock().await.get(&remote_media.stream_id)
                        {
                            process_remote_media(&remote_media, tracks).await;
                        }
                    }
                    Err(e) => error!("Error parsing message as RemoteMedia {e}")
                }
                
            })
        }));

        self.channels.lock().await.insert(label.to_owned(), ndc);

        Ok(())
    }

    pub async fn close(&self) -> Result<()> {
        if let Err(err) = self.pc.close().await {
            error!("subscriber peer close error: {err}");
        };
        Ok(())
    }

    pub async fn create_data_channel(&self, label: String) -> Result<Arc<RTCDataChannel>> {
        if let Some(channel) = self.channels.lock().await.get(&label) {
            return Ok(channel.clone());
        }
        let data_channel = self.pc.create_data_channel(label.as_str(), None).await?;
        info!("New data channel \"{}\" created for subscriber peer", label);
        self.channels
            .lock()
            .await
            .insert(label, data_channel.clone());

        Ok(data_channel)
    }

    pub async fn create_offer(&self) -> Result<RTCSessionDescription> {
        let offer = self.pc.create_offer(None).await?;
        self.pc.set_local_description(offer.clone()).await?;
        Ok(offer)
    }

    pub async fn add_down_track(&self, stream_id: String, down_track: Arc<DownTrack>) {
        info!("subscriber::add_down_track");
        let mut tracks = self.tracks.lock().await;
        if let Some(dt) = tracks.get_mut(&stream_id) {
            info!("subscriber:::add_down_track push into stream {stream_id}");
            dt.push(down_track);
            return;
        }
        info!("subscriber::add_down_track add stream {stream_id} with 0 tracks");
        tracks.insert(stream_id, Vec::new());
    }

    pub async fn add_ice_candidate(&self, candidate: RTCIceCandidateInit) -> Result<()> {
        if self.pc.remote_description().await.is_some() {
            self.pc.add_ice_candidate(candidate).await?;
            info!("subscriber::add_ice_candidate add candidate into peer connection");
            return Ok(());
        }
        info!("subscriber::add_ice_candidate add candidate into candidates vector");
        self.candidates.lock().await.push(candidate);
        Ok(())
    }

    pub async fn register_data_channel(&self, label: String, dc: Arc<RTCDataChannel>) {
        self.channels.lock().await.insert(label, dc);
    }

    pub fn register_on_ice_candidate(&self, f: OnLocalCandidateHdlrFn) {
        self.pc.on_ice_candidate(f)
    }

    pub async fn register_on_negociate(&self, f: OnNegotiateFn) {
        let mut handler = self.on_negotiate.lock().await;
        *handler = Some(f);
    }

    pub async fn data_channel(&self, label: &String) -> Option<Arc<RTCDataChannel>> {
        self.channels.lock().await.get(label).cloned()
    }

    pub async fn get_tracks(&self, stream_id: &String) -> Option<Vec<Arc<DownTrack>>> {
        self.tracks.lock().await.get(stream_id).cloned()
    }

    pub async fn negotiate(&self) -> Result<()> {
        info!("Calling negotitation");
        let mut handler = self.on_negotiate.lock().await;
        if let Some(f) = &mut *handler {
            f().await?;
        }
        Ok(())
    }

    async fn on_ice_connection_state_change(&self) {
        let pc_out = Arc::clone(&self.pc);

        self.pc.on_ice_connection_state_change(Box::new(
            move |ice_state: RTCIceConnectionState| {
                let pc_in = Arc::clone(&pc_out);
                Box::pin(async move {
                    match ice_state {
                        RTCIceConnectionState::Failed | RTCIceConnectionState::Closed => {
                            if let Err(e) = pc_in.close().await {
                                error!("on_ice_connection_state_change err: {}", e);
                            }
                        }
                        _ => {}
                    }
                })
            },
        ));
    }

    pub async fn remove_down_track(&self, stream_id: &String, down_track: &Arc<DownTrack>) {
        if let Some(dts) = self.tracks.lock().await.get_mut(stream_id) {
            dts.retain(|val| val.id() != down_track.id());
        }
    }

    pub async fn send_message(&self, content: &str) {
        for dc in self.channels.lock().await.values() {
            if let Err(e) = dc.send_text(content).await {
                error!("Send message error: {e}");
            };
        }
    }

    pub async fn send_stream_down_track_reports(&self, stream_id: &String) {
        let mut sds = Vec::new();
        let mut rtcp_packets: Vec<Box<(dyn webrtc::rtcp::packet::Packet + Send + Sync + 'static)>> =
            vec![];

        if let Some(dts) = self.tracks.lock().await.get(stream_id) {
            for dt in dts {
                if !dt.bound() {
                    continue;
                }
                if let Some(dcs) = dt.create_source_description_chunks().await {
                    sds.append(&mut dcs.clone());
                }
            }
        }

        if sds.is_empty() {
            return;
        }

        rtcp_packets.push(Box::new(SourceDescription { chunks: sds }));

        let pc_out = self.pc.clone();

        tokio::spawn(async move {
            let mut i = 0;
            loop {
                if let Err(err) = pc_out.write_rtcp(&rtcp_packets[..]).await {
                    log::error!("write rtcp error: {}", err);
                }

                if i > 5 {
                    return;
                }
                i += 1;

                sleep(Duration::from_millis(20)).await;
            }
        });
    }

    pub async fn set_remote_description(&self, sdp: RTCSessionDescription) -> Result<()> {
        self.pc.set_remote_description(sdp).await?;

        let candidates = self.candidates.lock().await;
        for candidate in &*candidates {
            self.pc.add_ice_candidate(candidate.clone()).await?;
        }

        self.candidates.lock().await.clear();

        info!("Answer accepted");
        Ok(())
    }
}

async fn process_remote_media(remote_media: &RemoteMedia, down_tracks: &Vec<Arc<DownTrack>>) {
    if let Some(layers) = &remote_media.layers {
        if !layers.is_empty() {
            return;
        }
    }
    for dt in down_tracks {
        match dt.kind() {
            RTPCodecType::Audio => dt.mute(!remote_media.audio),
            RTPCodecType::Video => {
                match remote_media.video.as_str() {
                    HIGH_VALUE => {
                        dt.mute(false);
                        if let Err(err) = dt.switch_spatial_layer(2, true).await {
                            error!("switch_spatial_layer err: {}", err);
                        }
                    }
                    MEDIA_VALUE => {
                        dt.mute(false);
                        if let Err(err) = dt.switch_spatial_layer(1, true).await {
                            error!("switch_spatial_layer err: {}", err);
                        }
                    }
                    LOW_VALUE => {
                        dt.mute(false);
                        if let Err(err) = dt.switch_spatial_layer(0, true).await {
                            error!("switch_spatial_layer err: {}", err);
                        }
                    }
                    MUTED_VALUE => {
                        dt.mute(true);
                    }
                    _ => {
                        warn!("remote_media.video \"{}\" unrecognized", remote_media.video);
                    }
                }

                match remote_media.frame_rate.as_str() {
                    HIGH_VALUE => dt.switch_temporal_layer(3, true).await,
                    MEDIA_VALUE => dt.switch_temporal_layer(2, true).await,
                    LOW_VALUE => dt.switch_temporal_layer(1, true).await,
                    _ => {}
                }
            }
            RTPCodecType::Unspecified => {}
        }
    }
}