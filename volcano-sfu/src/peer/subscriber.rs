use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use async_trait::async_trait;
use dashmap::DashMap;
use tokio::sync::Mutex;
use tokio::time::{Duration, sleep};
use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
use webrtc::ice_transport::ice_connection_state::RTCIceConnectionState;
use webrtc::ice_transport::ice_gatherer::OnLocalCandidateHdlrFn;
use webrtc::peer_connection::offer_answer_options::RTCOfferOptions;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtcp::source_description::SourceDescription;
use webrtc::rtp_transceiver::RTCRtpTransceiverInit;
use webrtc::rtp_transceiver::rtp_codec::{RTCRtpCodecCapability};
use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;
use webrtc::{data_channel::RTCDataChannel,
    ice_transport::ice_candidate::RTCIceCandidateInit, peer_connection::RTCPeerConnection,
};

use super::{OnOfferFn, api};
use crate::peer::consumer::Consumer;
use crate::session::config::WebRTCTransportConfig;
use crate::track::downtrack::{DownTrack, DownTrackInternal};
use crate::track::message::RemoteMedia;
use crate::track::receiver::WebRTCReceiver;

type Result<T> = core::result::Result<T, webrtc::Error>;

#[derive(Clone)]
pub struct Subscriber {
    pub id: String,
    pub pc: Arc<RTCPeerConnection>,
    //pub media_engine: Arc<Mutex<MediaEngine>>,

    api_channel: Arc<RTCDataChannel>,
    config: Arc<WebRTCTransportConfig>,
    tracks: DashMap<String, Arc<DownTrack>>,
    stream_tracks: DashMap<String, Vec<String>>,
    pub(crate) channels: DashMap<String, Arc<RTCDataChannel>>,
    candidates: Arc<Mutex<Vec<RTCIceCandidateInit>>>,
    //on_negotiate: Arc<Mutex<Option<OnNegotiateFn>>>,
    on_offer_fn: Arc<Mutex<Option<OnOfferFn>>>,
    on_renegotiate_fn: Arc<Mutex<Option<OnRenegotiateFn>>>,
    pub no_auto_subscribe: bool,
    negotiation_pending: Arc<AtomicBool>,
    api_channel_open: Arc<AtomicBool>,
    remote_answer_pending: Arc<AtomicBool>,
    session_version: Arc<AtomicU64>,
}

pub type OnRenegotiateFn = Box<
    dyn (FnMut(bool) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'static>>) + Send + Sync,
>;

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
            //media_engine: Default::default(),
            tracks: Default::default(),
            stream_tracks: Default::default(),
            channels: Default::default(),
            candidates: Default::default(),
            //on_negotiate: Default::default(),
            on_offer_fn: Default::default(),
            on_renegotiate_fn: Default::default(),
            no_auto_subscribe: Default::default(),
            negotiation_pending: Default::default(),
            remote_answer_pending: Default::default(),
            api_channel_open: open,
            session_version: Default::default(),
        };
        Ok(subscriber)
    }

    pub async fn add_data_channel(&self, label: &str) -> Result<()> {
        let ndc = self
            .pc
            .create_data_channel(label, Some(RTCDataChannelInit::default()))
            .await?;
        info!(
            "[{}] Created data channel `{}` (awaiting for offer)",
            self.id,
            ndc.label()
        );
        let tracks_map = self.tracks.clone();
        let stream_tracks = self.stream_tracks.clone();

        let ndc_1: Arc<RTCDataChannel> = ndc.clone();
        let ndc_2 = ndc.clone();
        ndc.on_open(Box::new(move || {
            Box::pin(async move {
                let _ = ndc_1
                    .send_text("{\"message\": \"Client should receive this message\"}")
                    .await;
            })
        }));
        ndc.on_message(Box::new(move |msg| {
            let data = String::from_utf8(msg.data.to_vec())
                .inspect_err(|_| error!("Error parsing message as string"))
                .unwrap();
            info!("[{}] Message received: {data}", ndc_2.label());
            let read_remote_media = serde_json::from_str::<RemoteMedia>(&data);
            let tracks_in = tracks_map.clone();
            let stream_tracks_in = stream_tracks.clone();

            Box::pin(async move {
                match read_remote_media {
                    Ok(remote_media) => {
                        // Get track IDs for the stream and collect the actual tracks
                        if let Some(track_ids) = stream_tracks_in.get(&remote_media.stream_id) {
                            let mut tracks = Vec::new();
                            for track_id in track_ids.value() {
                                if let Some(track) = tracks_in.get(track_id) {
                                    tracks.push(track.clone());
                                }
                            }
                            if !tracks.is_empty() {
                                api::process_remote_media(&remote_media, &tracks).await;
                            }
                        }
                    }
                    Err(e) => error!("Error parsing message as RemoteMedia {e}"),
                }
            })
        }));

        self.channels.insert(label.to_owned(), ndc);

        Ok(())
    }

    /// Sets remote description and, if there is a pending negotiation (or renegotiation), [Self::negotiate] is called.
    pub async fn on_remote_answer(&self, answer: RTCSessionDescription) -> Result<()> {
        info!("[Subscriber {}] sets remote description", self.id);
        self.pc.set_remote_description(answer).await?;
        self.remote_answer_pending.store(false, Ordering::Release);

        let mut candidates = self.candidates.lock().await;

        info!(
            "[Subscriber {}] ICE candidates ({})",
            self.id,
            candidates.len()
        );

        for candidate in candidates.drain(..) {
            if let Err(err) = self.pc.add_ice_candidate(candidate).await {
                warn!("add_ice_candidate error: {}", err);
            };
        }

        drop(candidates);

        if self.negotiation_pending.swap(false, Ordering::Relaxed) {
            info!("Negotiation pending. Start new negotiation.");
            self.negotiate(None).await?;
        }

        Ok(())
    }

    pub async fn close(&self) {
        if let Err(err) = self.pc.close().await {
            error!("subscriber peer close error: {err}");
        };
    }

    /// Crates a local data channel, and stores it by the provided label key.
    /// Side effect: Any existing channel with same label is closed.
    pub async fn create_data_channel(&self, label: String) -> Result<Arc<RTCDataChannel>> {
        let data_channel = self.pc.create_data_channel(label.as_str(), None).await?;
        info!("New data channel \"{}\" created for subscriber peer", label);
        if let Some(dc) = self.channels.get(&label) {
            // Close existing
            info!("Closing existing data channel");
            dc.close().await?;
        }

        self.channels.insert(label, data_channel.clone());

        Ok(data_channel)
    }

    /// Creates an offer for this peer connection and sets it as the local description.
    pub async fn create_offer(
        &self,
        options: Option<RTCOfferOptions>,
    ) -> Result<RTCSessionDescription> {
        let offer = self.pc.create_offer(options).await?;
        self.pc.set_local_description(offer.clone()).await?;
        if let Some(description) = offer.clone().unmarshal().ok() {
            self.session_version
                .store(description.origin.session_version, Ordering::Release);
        }
        Ok(offer)
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
        self.channels.insert(label, dc);
    }

    pub fn register_on_ice_candidate(&self, f: OnLocalCandidateHdlrFn) {
        self.pc.on_ice_candidate(f)
    }

    pub async fn register_on_renegotiate(&self, f: OnRenegotiateFn) {
        let mut handler = self.on_renegotiate_fn.lock().await;
        *handler = Some(f);
    }

    pub async fn data_channel(&self, label: &str) -> Option<Arc<RTCDataChannel>> {
        self.channels.get(label).map(|dc| dc.clone())
    }

    /// Get all tracks for a given stream ID
    pub fn get_tracks_by_stream(&self, stream_id: &str) -> Vec<Arc<DownTrack>> {
        if let Some(track_ids) = self.stream_tracks.get(stream_id) {
            track_ids
                .value()
                .iter()
                .filter_map(|track_id| self.tracks.get(track_id).map(|track| track.clone()))
                .collect()
        } else {
            Vec::new()
        }
    }

    pub async fn negotiate(&self, offer_options: Option<RTCOfferOptions>) -> Result<()> {
        debug!("Start negotiation");
        if self.remote_answer_pending.load(Ordering::Relaxed) {
            self.negotiation_pending.store(true, Ordering::Relaxed);
            debug!("Negotiation set to pending. Reason: Remote answer pending");
            return Ok(());
        }

        let offer = self.create_offer(offer_options).await?;
        self.remote_answer_pending.store(true, Ordering::Relaxed);
        let mut handler = self.on_offer_fn.lock().await;
        if let Some(f) = &mut *handler {
            f(offer).await;
        }
        Ok(())
    }

    /// Calls `on_negotiate` with the given ice_restart value
    /// # Returns
    /// Negotiation result
    pub async fn renegotiate(&self, ice_restart: bool) -> Result<()> {
        let options = Some(RTCOfferOptions {
            voice_activity_detection: true,
            ice_restart,
        });
        self.negotiate(options).await
    }

    pub async fn restart_peer_connection(
        &mut self,
        offer_options: Option<RTCOfferOptions>,
    ) -> Result<()> {
        let pc = self.pc.clone();
        match pc.close().await {
            Ok(_) => {
                info!("[Subscriber {}] Peer connection closed", self.id);
            }
            Err(err) => {
                warn!(
                    "[Subscriber {}] Peer connection close failed: {err}",
                    self.id
                );
            }
        }
        self.pc = api::create_subscriber_connection(&self.config).await?;
        self.negotiate(offer_options).await?;
        Ok(())
    }

    pub fn setup_renegotiation(self: &Arc<Self>) -> Result<()> {
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
        self.setup_ice_connection_state_change();
        Ok(())
    }

    fn setup_ice_connection_state_change(&self) {
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
                        }
                        RTCIceConnectionState::Closed => {
                            info!("[Subscriber {id}] ICE connection closed");
                        }
                        _ => {
                            debug!(
                                "[Subscriber {id}] ICE connection state changed to {:?}",
                                ice_state
                            );
                        }
                    }
                })
            },
        ));
    }

    pub async fn on_offer(&self, offer_fn: OnOfferFn) {
        let mut handler = self.on_offer_fn.lock().await;
        *handler = Some(offer_fn);
    }

    pub fn remove_down_track(&self, down_track: &Arc<DownTrack>) {
        let track_id = down_track.id().to_string();
        let stream_id = down_track.stream_id();

        // 1. Remove from tracks
        let removed = self.tracks.remove(&track_id).is_some();
        if !removed {
            return; // Track doesn't exist
        }

        info!(
            "[Subscriber {}] Removed track {} from stream {}",
            self.id, track_id, stream_id
        );

        // 2. Remove from stream mapping
        let should_remove_stream = {
            let mut guard = self.stream_tracks.get_mut(&stream_id);
            if let Some(track_ids) = guard.as_mut() {
                // Remove by index (more efficient than retain)
                if let Some(pos) = track_ids.iter().position(|id| id == &track_id) {
                    track_ids.swap_remove(pos);
                }
                track_ids.is_empty()
            } else {
                false
            }
        };

        if should_remove_stream {
            self.stream_tracks.remove(&stream_id);
        }
    }

    pub async fn send_message(&self, content: &str) -> Result<()> {
        let open = self.api_channel_open.load(Ordering::Acquire);
        if !open {
            return Err(webrtc::Error::ErrClosedPipe.into());
        }

        match self.api_channel.send_text(content).await {
            Ok(_) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn send_reports_by_stream(&self, stream_id: &String) {
        let mut sds = Vec::new();
        let mut rtcp_packets: Vec<Box<(dyn webrtc::rtcp::packet::Packet + Send + Sync + 'static)>> =
            vec![];

        // Use the efficient lock-free stream lookup
        let tracks = self.get_tracks_by_stream(stream_id);
        for dt in &tracks {
            if !dt.bound() {
                continue;
            }
            let mut chunks = dt.create_source_description_chunks().await;
            sds.append(&mut chunks);
        }

        if sds.is_empty() {
            return;
        }

        rtcp_packets.push(Box::new(SourceDescription { chunks: sds }));

        let pc_out = self.pc.clone();

        let id = self.id.clone();
        tokio::spawn(async move {
            for i in 1..=6 {
                debug!("[Subscriber {id}] Send source description ({i}/6)");
                if let Err(err) = pc_out.write_rtcp(&rtcp_packets[..]).await {
                    warn!("[Subscriber {id}] Send source description failed : {}", err);
                }

                sleep(Duration::from_millis(20)).await;
            }
        });
    }

    pub fn session_version(&self) -> u64 {
        self.session_version.load(Ordering::Acquire)
    }

    pub async fn set_remote_description(&self, sdp: RTCSessionDescription) -> Result<()> {
        self.pc.set_remote_description(sdp).await?;
        self.negotiation_pending.store(false, Ordering::Relaxed);

        let mut candidates = self.candidates.lock().await;

        info!(
            "[Subscriber {}] ICE candidates ({})",
            self.id,
            candidates.len()
        );
        for candidate in &*candidates {
            if let Err(err) = self.pc.add_ice_candidate(candidate.clone()).await {
                warn!("add_ice_candidate error: {}", err);
            };
        }

        candidates.clear();
        Ok(())
    }
}

#[async_trait]
impl Consumer for Subscriber {
    fn add_down_track(&self, down_track: Arc<DownTrack>) -> super::Result<()> {
        let stream_id = down_track.stream_id().to_owned();
        let dt_id = down_track.id();

        info!(
            "[Subscriber {}] add_down_track {} into stream {}",
            self.id, dt_id, stream_id
        );

        self.tracks.insert(dt_id.clone(), down_track);

        self.stream_tracks
            .entry(stream_id)
            .or_insert_with(Vec::new)
            .push(dt_id);

        Ok(())
    }

    fn down_track_by_id(&self, id: &str) -> Option<Arc<DownTrack>> {
        self.tracks.get(id).map(|track| track.clone())
    }

    fn id(&self) -> String {
        self.id.clone()
    }

    async fn new_local_track(
        &self,
        codec_capability: RTCRtpCodecCapability,
        receiver: &Arc<WebRTCReceiver>,
    ) -> super::Result<Arc<DownTrack>> {
        // New local down track
        let local_track = Arc::new(DownTrackInternal::new(
            codec_capability,
            receiver,
            self.config.router.max_packet_track,
        ));
        let transceiver = self
            .pc
            .add_transceiver_from_track(
                local_track.clone(),
                Some(RTCRtpTransceiverInit {
                    direction: RTCRtpTransceiverDirection::Sendonly,
                    send_encodings: Vec::new(),
                }),
            )
            .await?;
        // New local track
        let mut down_track = DownTrack::new_track_local(self.id(), local_track);
        down_track.set_transceiver(transceiver.clone());
        let down_track_arc = Arc::new(down_track);
        Ok(down_track_arc)
    }

    async fn unsubscribe_track(&self, track_id: &str) -> super::Result<()> {
        if self.pc.connection_state() == RTCPeerConnectionState::Closed {
            return Ok(()); // Nothing to unsubscribe
        }

        let down_track = match self.down_track_by_id(track_id) {
            Some(dt) => dt,
            None => return Ok(()), // Track already removed
        };

        let sender = match &down_track.transceiver {
            Some(t) => t.sender().await,
            None => {
                warn!("DownTrack {} has no transceiver", track_id);
                self.remove_down_track(&down_track);
                return Ok(());
            }
        };

        info!("[Subscriber {}] Remove DownTrack {}", self.id, down_track.id());
        self.remove_down_track(&down_track);
        drop(down_track); // Drop local track
        
        // Remove track sender from subscriber peer connection
        match self.pc.remove_track(&sender).await {
            Ok(_) => {

                // If remote answer is pending, set negotiation pending flag
                if self.remote_answer_pending.load(Ordering::Acquire) {
                    self.negotiation_pending.store(true, Ordering::Release);
                    return Ok(());
                }
                
                info!("[Subscriber {}] Track sender unsubscribed", self.id);
                let self_clone = Arc::new(self.clone());
                tokio::spawn(async move {
                    // Force negotiation
                    if let Err(err) = self_clone.negotiate(None).await {
                        warn!("Negotiation for track removed failed: {err}");
                    }
                });
            }
            Err(err) => {
                warn!("[Subscriber {}] Track sender unsubscribe failed: {err}", self.id);
            }
        };

        Ok(())
    }
}
