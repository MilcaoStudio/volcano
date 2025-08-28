use std::{
    ops::DerefMut, sync::{
        atomic::{AtomicBool, Ordering}, Arc
    }, time::Duration
};

use async_trait::async_trait;
use dashmap::DashMap;
use tokio::{sync::Mutex, time::sleep};
use webrtc::{
    data_channel::RTCDataChannel, ice_transport::{
        ice_candidate::RTCIceCandidateInit, ice_connection_state::RTCIceConnectionState,
    }, peer_connection::{
        configuration::RTCConfiguration, offer_answer_options::RTCOfferOptions, peer_connection_state::RTCPeerConnectionState, sdp::session_description::RTCSessionDescription, signaling_state::RTCSignalingState, OnDataChannelHdlrFn, RTCPeerConnection
    }, rtcp, rtp_transceiver::{
        rtp_codec::RTCRtpCodecCapability, rtp_transceiver_direction::RTCRtpTransceiverDirection, RTCRtpTransceiverInit
    }
};

use crate::{
    controllers::{PeerController, PeerControllerError, Result}, packet::AtomicFactory, peer::{
        api, Consumer, OnICECandidateFn, OnICEConnectionStateChangeFn, OnOfferFn, Result as PeerResult, API_CHANNEL_LABEL
    }, session::{config::WebRTCTransportConfig, room::Room}, track::{
        downtrack::{DownTrack, DownTrackInternal},
        message::RemoteMedia,
        receiver::{Receiver, WebRTCReceiver},
        router::LocalRouter,
    }
};

/// Peer consuming a [RTCPeerConnection] and a [RTCDataChannel]
#[derive(Clone, Default)]
pub struct CentralController {
    api_channel: Arc<Mutex<Option<Arc<RTCDataChannel>>>>,
    candidates: Arc<Mutex<Vec<RTCIceCandidateInit>>>,
    consumer: Arc<Mutex<Option<Arc<CentralConsumer>>>>,
    channels: Arc<DashMap<String, Arc<RTCDataChannel>>>,
    config: Arc<WebRTCTransportConfig>,
    closed: Arc<AtomicBool>,
    id: String,
    pub room: Arc<Mutex<Option<Arc<Room>>>>,
    #[allow(dead_code)]
    user_id: String,
    on_ice_candidate_fn: Arc<Mutex<Option<OnICECandidateFn>>>,
    on_ice_connection_state_change: Arc<Mutex<Option<OnICEConnectionStateChangeFn>>>,
    on_data_channel_fn: Arc<Mutex<Option<OnDataChannelHdlrFn>>>,
    on_offer_fn: Arc<Mutex<Option<OnOfferFn>>>,
    pc: Arc<Mutex<Option<Arc<RTCPeerConnection>>>>,
    //remote_answer_pending: Arc<AtomicBool>,
    router: Arc<Mutex<Option<Arc<LocalRouter>>>>,
    downtracks: Arc<DashMap<String, Vec<Arc<DownTrack>>>>,
    //negotiation_pending: Arc<AtomicBool>,
}

impl CentralController {
    /// Create a new Peer
    pub fn new(id: String, config: Arc<WebRTCTransportConfig>) -> Self {
        Self {
            config,
            id,
            ..Default::default()
        }
    }

    pub async fn add_data_channel(&self, channel: Arc<RTCDataChannel>) {
        let tracks_out = self.downtracks.clone();

        let ndc_1 = channel.clone();
        let ndc_2 = channel.clone();
        channel.on_open(Box::new(move || {
            Box::pin(async move {
                let _ = ndc_1
                    .send_text("{\"message\": \"Client should receive this message\"}")
                    .await;
            })
        }));
        channel.on_message(Box::new(move |msg| {
            let data = String::from_utf8(msg.data.to_vec())
                .inspect_err(|_| error!("Error parsing message as string"))
                .unwrap();
            info!("[{}] Message received: {data}", ndc_2.label());
            let read_remote_media = serde_json::from_str::<RemoteMedia>(&data);
            let tracks_in = tracks_out.clone();

            Box::pin(async move {
                match read_remote_media {
                    Ok(remote_media) => {
                        if let Some(tracks) = tracks_in.get(&remote_media.stream_id) {
                            api::process_remote_media(&remote_media, &tracks).await;
                        }
                    }
                    Err(e) => warn!("Error parsing message as RemoteMedia {e}"),
                }
            })
        }));
        self.channels.insert(channel.label().to_owned(), channel);
    }

    async fn add_down_track(self: &Arc<Self>, receiver: Arc<WebRTCReceiver>) -> Result<()> {
        let tracks = self.downtracks.get(&receiver.stream_id());
        // Checks for available tracks
        if let Some(downtracks) = tracks {
            if downtracks.iter().any(|dt| dt.id() == receiver.track_id()) {
                info!("[Peer {}] add_down_track Downtrack exists", self.id);
                return Ok(());
            }
        }

        let codec_capability = receiver.codec().capability;
        let down_track_local = Arc::new(DownTrackInternal::new(
            codec_capability,
            &receiver,
            self.config.router.max_packet_track as u16,
            self.config.factory.clone(),
        ));

        let transceiver = match &*self.pc.lock().await {
            Some(pc) => {
                // This peer must send and receive RTP packets
                pc.add_transceiver_from_track(
                    down_track_local.clone(),
                    Some(RTCRtpTransceiverInit {
                        direction: RTCRtpTransceiverDirection::Sendrecv,
                        send_encodings: Vec::default(),
                    }),
                )
                .await?
            }
            None => {
                return Err(PeerControllerError::ErrPeer(
                    crate::peer::Error::ErrNoTransportEstablished,
                ));
            }
        };

        // New local track
        let down_track = DownTrack::new_track_local(self.id.clone(), down_track_local);
        info!(
            "[Peer {}] add_down_track New local track created {}",
            self.id,
            down_track.id()
        );
        down_track.set_transceiver(transceiver.clone()).await;
        let down_track_arc = Arc::new(down_track);

        let peer_1 = self.clone();
        let down_track_1 = down_track_arc.clone();
        let receiver_1 = receiver.clone();
        let layer = receiver.get_available_layer(self.config.router.simulcast.best_quality_first).await;
        down_track_arc
            .on_close(Box::new(move || {
                let dt_in = down_track_1.clone();
                let receiver_in = receiver_1.clone();
                let peer_in = peer_1.clone();
                let transceiver_in = transceiver.clone();
                Box::pin(async move {
                    if let Some(pc) = &*peer_in.pc.lock().await {
                        if let Err(err) = pc.remove_track(&transceiver_in.sender().await).await {
                            error!(
                                "[Peer {}] Remove sender from peer connection failed: {}",
                                peer_in.id, err
                            );
                            return;
                        };
                    }

                    peer_in.remove_down_track(&receiver_in.stream_id(), &dt_in.id());
                    if let Err(err) = peer_in.negotiate(None).await {
                        error!("[Peer {}] negotiate err:{} ", peer_in.id, err);
                    }
                })
            }))
            .await;

        let peer_2 = self.clone();
        let stream_id = receiver.stream_id();
        down_track_arc
            .on_bind(Box::new(move || {
                let peer_in = peer_2.clone();
                let s_id = stream_id.clone();
                Box::pin(async move {
                    peer_in.send_stream_down_track_reports(&s_id).await;
                })
            }))
            .await;

        let stream_id = receiver.stream_id();
        let streams = self
            .downtracks
            .entry(stream_id)
            .and_modify(|dts| dts.push(down_track_arc.clone()))
            .or_insert(vec![down_track_arc.clone()]);
        info!(
            "[Peer {}] add_down_track Down tracks updated {:?}",
            self.id, streams
        );

        match receiver
            .add_down_track(
                down_track_arc.clone(),
                layer,
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(e) => {
                error!("add_down_track failed {e}");
                Err(PeerControllerError::ErrReceiverClosed)
            }
        }
    }

    pub async fn join(self: &Arc<Self>, room: Arc<Room>) -> Result<()> {
        let id = &self.id;
        info!("[{id}] Join to {} requested", room.id);
        *self.room.lock().await = Some(room.clone());

        let mut peer_config = (*self.config).clone();
        {
            peer_config.configuration = RTCConfiguration {
                ice_servers: self.config.configuration.ice_servers.clone(),
                ..Default::default()
            };
            peer_config.factory = Arc::default();
        }

        let router = Arc::new(LocalRouter::new(
            self.id.clone(),
            Arc::downgrade(&room),
            self.config.router.clone(),
        ));
        {
            let packet_capacity = peer_config.router.max_packet_track as u16;
            *self.router.lock().await = Some(router);
            let pc = api::create_central_connection(peer_config.into()).await?;
            self.register_handlers(&pc).await;
            let consumer = CentralConsumer::new(self.id.clone(), pc.clone(), packet_capacity);
            self.setup_consumer(&consumer).await;
            *self.consumer.lock().await = Some(consumer);
            *self.pc.lock().await = Some(pc);
        }

        Ok(())
    }

    async fn register_handlers(self: &Arc<Self>, pc: &Arc<RTCPeerConnection>) {
        let on_ice_candidate_out = self.on_ice_candidate_fn.clone();
        let closed_out = self.closed.clone();
        pc.on_ice_candidate(Box::new(move |candidate| {
            let handler_in = on_ice_candidate_out.clone();
            let closed_in = closed_out.clone();
            Box::pin(async move {
                match candidate {
                    Some(candidate) => {
                        if let Some(handler) = &mut *handler_in.lock().await {
                            if !closed_in.load(Ordering::Relaxed) {
                                if let Ok(val) = candidate.to_json() {
                                    handler(val).await;
                                }
                            }
                        }
                    }
                    None => {}
                }
            })
        }));

        let on_ice_connection_state_change = self.on_ice_connection_state_change.clone();
        let id_1 = self.id.clone();
        pc.on_ice_connection_state_change(Box::new(move |state| {
            let handler = on_ice_connection_state_change.clone();
            let id = id_1.clone();
            Box::pin(async move {
                if let Some(h) = &mut *handler.lock().await {
                    h(state).await;
                }
                match state {
                    RTCIceConnectionState::Closed => {
                        warn!(
                            "[Peer {}] Peer ICE connection closed. No close handler available",
                            id
                        );
                    }
                    _ => {
                        debug!(
                            "[Peer {}] Peer ICE connection state changed to: {}",
                            id, state
                        );
                    }
                }
            })
        }));
        match &*self.router.lock().await {
            Some(router) => {
                self.on_track(pc, router.clone());
            }
            _ => {
                error!("[Peer {}] No router available", self.id);
            }
        }

        let peer_1 = self.clone();
        let id = self.id.clone();
        let on_data_channel_fn = self.on_data_channel_fn.clone();
        pc.on_data_channel(Box::new(move |data_channel| {
            let id_in = id.clone();
            let peer = peer_1.clone();
            let handler_in = on_data_channel_fn.clone();
            Box::pin(async move {
                let label = data_channel.label();
                if label == API_CHANNEL_LABEL {
                    info!("[Peer {}] API data channel open", id_in);
                    *peer.api_channel.lock().await = Some(data_channel.clone());
                } else {
                    info!("[Peer {}] Data channel `{}` open", id_in, label);
                    peer.add_data_channel(data_channel.clone()).await;
                }

                // Call the user-provided callback if it exists
                if let Some(handler) = handler_in.lock().await.as_mut() {
                    handler(data_channel).await;
                }
            })
        }));
    }

    async fn setup_consumer(&self, consumer: &Arc<CentralConsumer>) {
        let offer_fn_out = self.on_offer_fn.clone();
        consumer
            .on_offer(Box::new(move |offer| {
                let handler_in = offer_fn_out.clone();
                Box::pin(async move {
                    if let Some(handler) = handler_in.lock().await.as_mut() {
                        handler(offer).await;
                    }
                })
            }))
            .await;
    }

    fn on_track(self: &Arc<Self>, pc: &Arc<RTCPeerConnection>, router: Arc<LocalRouter>) {
        let peer_1 = self.clone();
        pc.on_track(Box::new(move |track, receiver, _| {
            let router_in = router.clone();
            let peer_in = peer_1.clone();
            Box::pin(async move {
                let (r, _) = router_in.add_uptrack(receiver, track.clone()).await;
                if let Err(err) = peer_in.add_down_track(r).await {
                    error!(
                        "[Peer {}] on_track Add down track failed: {}",
                        peer_in.id, err
                    );
                };
            })
        }));
    }

    fn remove_down_track(&self, stream_id: &str, down_track_id: &str) {
        if let Some(mut dts) = self.downtracks.get_mut(stream_id) {
            dts.retain(|val| val.id() != down_track_id);
        }
    }

    async fn send_stream_down_track_reports(&self, stream_id: &String) {
        use webrtc::rtcp::{packet::Packet, source_description::SourceDescription};
        let mut sds = Vec::new();
        let mut rtcp_packets: Vec<Box<(dyn Packet + Send + Sync + 'static)>> = Vec::default();

        if let Some(dts) = self.downtracks.get(stream_id) {
            for dt in dts.iter() {
                if !dt.bound() {
                    continue;
                }
                let mut dcs = dt.create_source_description_chunks();
                sds.append(&mut dcs);
            }
        }

        if sds.is_empty() {
            return;
        }

        rtcp_packets.push(Box::new(SourceDescription { chunks: sds }));

        let id = self.id.clone();

        match &*self.pc.lock().await {
            Some(pc) => {
                let pc_out = pc.clone();
                tokio::spawn(async move {
                    for i in 1..6 {
                        debug!("[Peer {id}] Send source description ({i}/6)");
                        if let Err(err) = pc_out.write_rtcp(&rtcp_packets[..]).await {
                            warn!("write rtcp error: {}", err);
                        }

                        sleep(Duration::from_millis(20)).await;
                    }
                });
            }
            _ => {
                warn!("[Peer {id}] send_stream_down_track_reports No peer connection found");
            }
        };
    }

    // Helper methods that are used by the public methods but are not part of the trait
    pub async fn add_ice_candidate(&self, candidate: RTCIceCandidateInit) -> Result<()> {
        match &*self.pc.lock().await {
            Some(pc) => {
                if pc.remote_description().await.is_some() {
                    pc.add_ice_candidate(candidate.clone()).await?;
                    info!("publisher::add_ice_candidate add candidate into peer connection");
                    return Ok(());
                }

                info!("publisher::add_ice_candidate add candidate into candidates vector");
                self.candidates.lock().await.push(candidate.clone());

                Ok(())
            }
            None => Err(PeerControllerError::ErrPeer(
                crate::peer::Error::ErrNoTransportEstablished,
            )),
        }
    }

    pub async fn answer(&self, sdp: RTCSessionDescription) -> Result<RTCSessionDescription> {
        info!("[Peer {}] Got offer", self.id);
        match &*self.pc.lock().await {
            Some(pc) => {
                if pc.signaling_state() != RTCSignalingState::Stable {
                    return Err(PeerControllerError::ErrPeer(
                        crate::peer::Error::ErrOfferIgnored,
                    ));
                }
                pc.set_remote_description(sdp).await?;

                let mut candidates = self.candidates.lock().await;
                for c in &*candidates {
                    if let Err(err) = pc.add_ice_candidate(c.clone()).await {
                        warn!(
                            "[Peer {}] Candidate {} could not be added: {}",
                            self.id, c.candidate, err
                        );
                    }
                }
                let answer = pc.create_answer(None).await?;
                pc.set_local_description(answer.clone()).await?;
                candidates.clear();
                Ok(answer)
            }
            None => Err(PeerControllerError::ErrPeer(
                crate::peer::Error::ErrNoTransportEstablished,
            )),
        }
    }

    pub async fn set_on_ice_candidate(&self, f: OnICECandidateFn) {
        *self.on_ice_candidate_fn.lock().await = Some(f);
    }

    pub async fn set_on_ice_connection_state_change(&self, f: OnICEConnectionStateChangeFn) {
        *self.on_ice_connection_state_change.lock().await = Some(f);
    }
}

type WebRTCResult<T> = core::result::Result<T, webrtc::Error>;

#[async_trait]
impl PeerController for CentralController {
    /// Cleans up the peer connection and closes it.
    async fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
        {
            let pc = self.pc.lock().await.take();
            if let Some(pc) = pc {
                if let Err(err) = pc.close().await {
                    error!("[Peer {}] Closing peer connection failed: {err}", self.id);
                }
            }
        }
    }

    /// Closes the data channel related to the `label`, if exists.
    async fn close_local_data_channel(&self, label: &str) -> Result<()> {
        if let Some((_, channel)) = self.channels.remove(label) {
            channel.close().await?;
        }
        Ok(())
    }

    async fn consumer(&self) -> Option<Arc<dyn Consumer + Send + Sync>> {
        match self.consumer.lock().await.as_ref() {
            Some(consumer)=> Some(consumer.clone()),
            None => None,
        }
    }

    /// Creates a [RTCDataChannel] from a label, and stores it.
    async fn create_local_data_channel(&self, label: String) -> Result<Arc<RTCDataChannel>> {
        let channels = &self.channels;
        if let Some(channel) = channels.get(&label) {
            return Ok(channel.clone());
        }

        match self.pc.lock().await.as_deref() {
            Some(pc) => {
                let data_channel = pc.create_data_channel(label.as_str(), None).await?;
                info!("[{0}] Data channel \"{1}\" created", self.user_id, label);
                channels.insert(label, data_channel.clone());

                Ok(data_channel)
            }
            _ => Err(PeerControllerError::ErrPeer(
                crate::peer::Error::ErrNoTransportEstablished,
            )),
        }
    }

    /// Peer Id
    fn id(&self) -> String {
        self.id.clone()
    }

    /// The [RTCDataChannel] corresponding to `label`.
    async fn local_data_channel(&self, label: &str) -> Option<Arc<RTCDataChannel>> {
        self.channels.get(label).map(|channel| channel.clone())
    }

    /// Starts negotiation process.
    async fn negotiate(&self, offer_options: Option<RTCOfferOptions>) -> Result<()> {
        match self.consumer.lock().await.as_ref() {
            Some(consumer) => consumer.negotiate(offer_options).await.map_err(Into::into),
            _ => Err(PeerControllerError::ErrNoConsumer),
        }
    }

    /// Sets a function to be called when a remote data channel is opened.
    async fn on_data_channel(&self, f: OnDataChannelHdlrFn) {
        *self.on_data_channel_fn.lock().await = Some(f);
    }

    /// Sets remote description for this peer. If this fails, a new offer should be created from client side.
    async fn on_remote_answer(&self, sdp: RTCSessionDescription) -> Result<()> {
        match self.consumer.lock().await.as_ref() {
            Some(consumer) => {
                let mut candidates = self.candidates.lock().await;
                consumer.on_remote_answer(sdp, candidates.deref_mut()).await.map_err(Into::into)
            }
            None => Err(PeerControllerError::ErrNoConsumer),
        }
    }

    /// Sets remote and local descriptions for this peer. If this fails, a new offer should be created from client side.
    async fn on_remote_offer(&self, sdp: RTCSessionDescription) -> Result<RTCSessionDescription> {
        info!("[Peer {}] Got offer", self.id);
        match &*self.pc.lock().await {
            Some(pc) => {
                if pc.signaling_state() != RTCSignalingState::Stable {
                    return Err(PeerControllerError::ErrPeer(
                        crate::peer::Error::ErrOfferIgnored,
                    ));
                }
                pc.set_remote_description(sdp).await?;

                let mut candidates = self.candidates.lock().await;
                for c in &*candidates {
                    if let Err(err) = pc.add_ice_candidate(c.clone()).await {
                        warn!(
                            "[Peer {}] Candidate {} could not be added: {}",
                            self.id, c.candidate, err
                        );
                    }
                }
                let answer = pc.create_answer(None).await?;
                pc.set_local_description(answer.clone()).await?;
                candidates.clear();
                Ok(answer)
            }
            None => Err(PeerControllerError::ErrPeer(
                crate::peer::Error::ErrNoTransportEstablished,
            )),
        }
    }

    /// Sets a function to be called when current peer connection's state changes.
    async fn on_ice_connection_state_change(&self, f: OnICEConnectionStateChangeFn) {
        *self.on_ice_connection_state_change.lock().await = Some(f);
    }

    /// Peer router used for track forwarding to another peer
    async fn router(&self) -> Result<Arc<LocalRouter>> {
        match &*self.router.lock().await {
            Some(router) => Ok(router.clone()),
            None => Err(PeerControllerError::ErrPeer(
                crate::peer::Error::ErrNoTransportEstablished,
            )),
        }
    }
}

#[derive(Clone)]
struct CentralConsumer {
    id: String,
    negotiation_pending: Arc<AtomicBool>,
    on_offer_fn: Arc<Mutex<Option<OnOfferFn>>>,
    // Peer connection must exist
    pc: Arc<RTCPeerConnection>,
    rtp_packet_capacity: u16,
    remote_answer_pending: Arc<AtomicBool>,
    tracks: DashMap<String, Arc<DownTrack>>,
}

impl CentralConsumer {
    pub fn new(id: String, pc: Arc<RTCPeerConnection>, capacity: u16) -> Arc<Self> {
        Arc::new(Self {
            id,
            negotiation_pending: Default::default(),
            on_offer_fn: Default::default(),
            pc,
            remote_answer_pending: Default::default(),
            rtp_packet_capacity: capacity,
            tracks: Default::default(),
        })
    }

    /// Creates an offer for this peer connection and sets it as the local description.
    async fn create_offer(
        &self,
        options: Option<RTCOfferOptions>,
    ) -> WebRTCResult<RTCSessionDescription> {
        let offer = self.pc.create_offer(options).await?;
        self.pc.set_local_description(offer.clone()).await?;
        Ok(offer)
    }

    /// Starts negotiation process.
    async fn negotiate(&self, offer_options: Option<RTCOfferOptions>) -> WebRTCResult<()> {
        debug!("Start negotiation");
        if self.remote_answer_pending.load(Ordering::Acquire) {
            self.negotiation_pending.store(true, Ordering::Release);
            debug!("Negotiation set to pending. Reason: Remote answer pending");
            return Ok(());
        }

        let offer = self.create_offer(offer_options).await?;
        self.remote_answer_pending.store(true, Ordering::Release);

        if let Some(on_offer) = &mut *self.on_offer_fn.lock().await {
            info!("[Peer {}] Send offer", self.id);
            on_offer(offer).await;
        };
        Ok(())
    }

    pub async fn on_offer(&self, f: OnOfferFn) {
        let mut handler = self.on_offer_fn.lock().await;
        *handler = Some(f);
    }

    pub async fn on_remote_answer(&self, answer: RTCSessionDescription, candidates: &mut Vec<RTCIceCandidateInit>) -> WebRTCResult<()> {
        self.pc.set_remote_description(answer).await?;
        self.remote_answer_pending.store(false, Ordering::Relaxed);

        for c in candidates.drain(..) {
            if let Err(err) = self.pc.add_ice_candidate(c).await {
                warn!(
                    "[Peer {}] Candidate could not be added: {}",
                    self.id, err
                );
            }
        }

        // Check if there's a pending negotiation
        if self.negotiation_pending.load(Ordering::Relaxed) {
            self.negotiation_pending.store(false, Ordering::Relaxed);
            self.negotiate(None).await?;
        }

        Ok(())
    }

    pub fn remove_down_track(&self, down_track: &Arc<DownTrack>) {
        let track_id = down_track.id().to_string();
        let stream_id = down_track.stream_id();

        // 1. Remove from tracks
        let removed = self.tracks.remove(&track_id).is_some();
        if !removed {
            return;
        }

        info!(
            "[Consumer {}] Removed track {} from stream {}",
            self.id, track_id, stream_id
        );
    }
}

#[async_trait]
impl Consumer for CentralConsumer {
    fn add_down_track(&self, down_track: Arc<DownTrack>) {
        self.tracks.insert(down_track.id(), down_track);
    }

    fn down_track_by_id(&self, track_id: &str) -> Option<Arc<DownTrack>> {
        self.tracks.get(track_id).map(|dt| dt.clone())
    }
    
    fn id(&self) -> String {
        self.id.clone()
    }

    async fn new_local_track(
        &self,
        codec_capability: RTCRtpCodecCapability,
        receiver: &Arc<WebRTCReceiver>,
        factory: Arc<AtomicFactory>,
    ) -> PeerResult<Arc<DownTrack>> {
        // New local down track
        let local_track = Arc::new(DownTrackInternal::new(
            codec_capability,
            receiver,
            self.rtp_packet_capacity,
            factory,
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
        let down_track = DownTrack::new_track_local(self.id.clone(), local_track);
        down_track.set_transceiver(transceiver.clone()).await;
        let down_track_arc = Arc::new(down_track);
        Ok(down_track_arc)
    }

    async fn unsubscribe_track(&self, track_id: &str) -> PeerResult<()> {
        if self.pc.connection_state() == RTCPeerConnectionState::Closed {
            return Ok(()); // Nothing to unsubscribe
        }

        let down_track = match self.down_track_by_id(track_id) {
            Some(dt) => dt,
            None => return Ok(()), // Track already removed
        };

        let sender = match &*down_track.transceiver.read().await {
            Some(t) => t.sender().await,
            None => {
                warn!("DownTrack {} has no transceiver", track_id);
                self.remove_down_track(&down_track);
                return Ok(());
            }
        };

        info!(
            "[Consumer {}] Remove DownTrack {}",
            self.id,
            down_track.id()
        );
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
                warn!(
                    "[Subscriber {}] Track sender unsubscribe failed: {err}",
                    self.id
                );
            }
        };

        Ok(())
    }

    async fn write_rtcp(&self, pkts: Vec<Box<dyn rtcp::packet::Packet + Send + Sync>>) -> PeerResult<()> {
        self.pc.write_rtcp(&pkts[..]).await?;
        Ok(())
    }
}
