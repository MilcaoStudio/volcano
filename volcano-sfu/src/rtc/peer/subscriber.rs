use std::pin::Pin;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::{collections::HashMap, sync::Arc};

use tokio::sync::Mutex;
use tokio::time::{sleep, Duration};
use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
use webrtc::ice_transport::ice_connection_state::RTCIceConnectionState;
use webrtc::ice_transport::ice_gatherer::OnLocalCandidateHdlrFn;
use webrtc::peer_connection::offer_answer_options::RTCOfferOptions;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtcp::source_description::SourceDescription;
use webrtc::{
    api::media_engine::MediaEngine, data_channel::RTCDataChannel,
    ice_transport::ice_candidate::RTCIceCandidateInit, peer_connection::RTCPeerConnection,
};

use super::{api, OnNegotiateFn, error::Result};
use crate::rtc::config::WebRTCTransportConfig;
use crate::rtc::message::RemoteMedia;
use crate::track::downtrack::DownTrack;

pub struct Subscriber {
    pub id: String,
    pub pc: Arc<RTCPeerConnection>,
    pub m: Arc<Mutex<MediaEngine>>,

    api_channel: Arc<RTCDataChannel>,
    config: Arc<WebRTCTransportConfig>,
    tracks: Arc<Mutex<HashMap<String, Vec<Arc<DownTrack>>>>>,
    channels: Arc<Mutex<HashMap<String, Arc<RTCDataChannel>>>>,
    candidates: Arc<Mutex<Vec<RTCIceCandidateInit>>>,
    on_negotiate: Arc<Mutex<Option<OnNegotiateFn>>>,
    on_renegotiate: Arc<Mutex<Option<OnRenegotiateFn>>>,
    pub no_auto_subscribe: bool,
    negotiation_pending: AtomicBool,
    api_channel_open: Arc<AtomicBool>,
}

pub type OnRenegotiateFn =
    Box<dyn (FnMut(bool) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'static>>) + Send + Sync>;

impl Subscriber {
    pub async fn new(id: String, config: Arc<WebRTCTransportConfig>) -> Result<Self> {
        let pc = api::create_subscriber_connection(&config.clone()).await?;
        let open = Arc::new(AtomicBool::default());
        let api_channel = api::create_api_data_channel(&pc, id.clone(), open.clone()).await?;

        let subscriber = Subscriber {
            api_channel,
            config,
            id,
            pc,
            m: Default::default(),
            tracks: Default::default(),
            channels: Default::default(),
            candidates: Default::default(),
            on_negotiate: Default::default(),
            on_renegotiate: Default::default(),
            no_auto_subscribe: Default::default(),
            negotiation_pending: Default::default(),
            api_channel_open: open,
        };
        Ok(subscriber)
    }

    pub async fn add_data_channel(&self, label: &str) -> Result<()> {
        let ndc = self
            .pc
            .create_data_channel(label, Some(RTCDataChannelInit::default()))
            .await?;
        info!("[{}] Created data channel `{}` (awaiting for offer)", self.id, ndc.label());
        let tracks_out = self.tracks.clone();

        let ndc_1: Arc<RTCDataChannel> = ndc.clone();
        let ndc_2 = ndc.clone();
        ndc.on_open(Box::new(move || {
            Box::pin(async move {
                let _ = ndc_1.send_text("{\"message\": \"Client should receive this message\"}").await;
            })
        }));
        ndc.on_message(Box::new(move |msg| {
            let data = String::from_utf8(msg.data.to_vec())
                .inspect_err(|_| error!("Error parsing message as string"))
                .unwrap();
            info!("[{}] Message received: {data}", ndc_2.label());
            let read_remote_media = serde_json::from_str::<RemoteMedia>(&data);
            let tracks_in = tracks_out.clone();
            
            Box::pin(async move {
                
                match read_remote_media {
                    Ok(remote_media) => {
                        if let Some(tracks) =
                            tracks_in.lock().await.get(&remote_media.stream_id)
                        {
                            api::process_remote_media(&remote_media, tracks).await;
                        }
                    }
                    Err(e) => error!("Error parsing message as RemoteMedia {e}")
                }
                
            })
        }));

        self.channels.lock().await.insert(label.to_owned(), ndc);

        Ok(())
    }

    pub async fn close(&self) {
        if let Err(err) = self.pc.close().await {
            error!("subscriber peer close error: {err}");
        };
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

    /// Creates an offer for this peer connection and sets it as the local description.
    pub async fn create_offer(&self, options: Option<RTCOfferOptions>) -> Result<RTCSessionDescription> {
        let offer = self.pc.create_offer(options).await?;
        self.pc.set_local_description(offer.clone()).await?;
        Ok(offer)
    }

    pub async fn add_down_track(&self, stream_id: String, down_track: Arc<DownTrack>) {
        let id = &self.id;
        let dt_id = down_track.id();
        let mut tracks = self.tracks.lock().await;
        let vec = tracks.entry(stream_id.clone()).or_insert(Vec::default());
        info!("[Subscriber {id}] add_down_track {dt_id} into stream {stream_id}");
        vec.push(down_track);
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

    pub fn api_channel(&self) -> Arc<RTCDataChannel> {
        self.api_channel.clone()
    }

    pub async fn register_data_channel(&self, label: String, dc: Arc<RTCDataChannel>) {
        self.channels.lock().await.insert(label, dc);
    }

    pub fn register_on_ice_candidate(&self, f: OnLocalCandidateHdlrFn) {
        self.pc.on_ice_candidate(f)
    }

    pub async fn register_on_negotiate(&self, f: OnNegotiateFn) {
        let mut handler = self.on_negotiate.lock().await;
        *handler = Some(f);
    }

    pub async fn register_on_renegotiate(&self, f: OnRenegotiateFn) {
        let mut handler = self.on_renegotiate.lock().await;
        *handler = Some(f);
    }

    pub async fn data_channel(&self, label: &String) -> Option<Arc<RTCDataChannel>> {
        self.channels.lock().await.get(label).cloned()
    }

    pub async fn get_tracks(&self, stream_id: &String) -> Option<Vec<Arc<DownTrack>>> {
        self.tracks.lock().await.get(stream_id).cloned()
    }

    pub async fn negotiate(&self, offer_options: Option<RTCOfferOptions>) -> Result<()> {
        let mut handler = self.on_negotiate.lock().await;
        if let Some(f) = &mut *handler {
            f(offer_options).await?;
        }
        Ok(())
    }

    /// Calls `on_negotiate` with the given ice_restart value
    /// # Returns
    /// Negotiation result
    pub async fn renegotiate(&self, ice_restart: bool) -> Result<()> {
        let options = Some(RTCOfferOptions { voice_activity_detection: true, ice_restart });
        self.negotiate(options).await
    }

    pub async fn restart_peer_connection(&mut self, offer_options: Option<RTCOfferOptions>) -> Result<()> {
        let pc = self.pc.clone();
        match pc.close().await {
            Ok(_) => {
                info!("[Subscriber {}] Peer connection closed", self.id);
            },
            Err(err) => {
                warn!("[Subscriber {}] Peer connection close failed: {err}", self.id);
            }
        }
        self.pc = api::create_subscriber_connection(&self.config).await?;
        self.negotiate(offer_options).await?;
        Ok(())
    }

    pub fn on_answer(self: &Arc<Self>) -> Result<()> {
        let sub = self.clone();
        self.pc.on_negotiation_needed(Box::new(move || {
            let sub_in = sub.clone();
            Box::pin(async move {
                info!("Start renegotiation");
                if let Err(err) = sub_in.renegotiate(true).await {
                    error!("renegotiate err: {}", err);
                }
            })
        }));
        self.on_ice_connection_state_change();
        Ok(())
    }

    fn on_ice_connection_state_change(&self) {
        let pc_out = Arc::clone(&self.pc);
        let id_out = self.id.clone();

        self.pc.on_ice_connection_state_change(Box::new(
            move |ice_state: RTCIceConnectionState| {
                let pc_in = Arc::clone(&pc_out);
                let id = id_out.clone();
                Box::pin(async move {
                    match ice_state {
                        RTCIceConnectionState::Failed => {
                            info!("[Subscriber {id}] Restarting ICE");
                            if let Err(err) = pc_in.restart_ice().await {
                               error!("[Subscriber {id}] restart_ice err: {}", err);
                            }
                        },
                        RTCIceConnectionState::Closed => {
                            info!("[Subscriber {id}] ICE connection closed");
                        },
                        _ => {
                            debug!("[Subscriber {id}] ICE connection state changed to {:?}", ice_state);
                        }
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
        let open = self.api_channel_open.load(Ordering::Acquire);
        if !open {
            return;
        }

        if let Err(e) = self.api_channel.send_text(content).await {
            match e {
                webrtc::Error::ErrDataChannelNotOpen | webrtc::Error::ErrClosedPipe => {
                    let pending = self.negotiation_pending.load(Ordering::Relaxed);
                    if !pending {
                        match self.renegotiate(true).await {
                            Ok(_) => self.negotiation_pending.store(true, Ordering::Relaxed),
                            Err(err) => error!("[Subscriber {}] [send_message] negotiate error: {err}", self.id),
                        }
                    } 
                },
                _ => error!("[Subscriber {}] Send message error: {e}", self.id),
            }
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
                if let Some(dcs) = dt.create_source_description_chunks().await.as_mut() {
                    sds.append(dcs);
                }
            }
        }

        if sds.is_empty() {
            return;
        }

        rtcp_packets.push(Box::new(SourceDescription { chunks: sds }));

        let pc_out = self.pc.clone();

        let id = self.id.clone();
        tokio::spawn(async move {
            for i in 1..6 {
                debug!("[Subscriber {id}] Send source description ({i}/6)");
                if let Err(err) = pc_out.write_rtcp(&rtcp_packets[..]).await {
                    warn!("write rtcp error: {}", err);
                }

                sleep(Duration::from_millis(20)).await;
            }
        });
    }

    pub async fn set_remote_description(&self, sdp: RTCSessionDescription) -> Result<()> {
        self.pc.set_remote_description(sdp).await?;
        self.negotiation_pending.store(false, Ordering::Relaxed);

        let mut candidates = self.candidates.lock().await;
        
        info!("[Subscriber {}] ICE candidates ({})", self.id, candidates.len());
        for candidate in &*candidates {
            if let Err(err) = self.pc.add_ice_candidate(candidate.clone()).await {
                warn!("add_ice_candidate error: {}", err);
            };
        }
        
        candidates.clear();
        Ok(())
    }
}


