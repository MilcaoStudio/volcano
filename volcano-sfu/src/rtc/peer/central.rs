use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use dashmap::DashMap;
use tokio::{sync::Mutex, time::sleep};
use webrtc::{
    data_channel::RTCDataChannel,
    ice_transport::{ice_candidate::RTCIceCandidateInit, ice_connection_state::RTCIceConnectionState},
    peer_connection::{
        configuration::RTCConfiguration, offer_answer_options::RTCOfferOptions, sdp::session_description::RTCSessionDescription, signaling_state::RTCSignalingState, RTCPeerConnection
    },
    rtp_transceiver::{
        rtp_transceiver_direction::RTCRtpTransceiverDirection, RTCRtpTransceiverInit
    },
};

use super::{
    OnICECandidateFn, OnICEConnectionStateChangeFn, OnOfferFn, Peer, api,
    error::{Error, Result},
};
use crate::{
    rtc::{
        config::WebRTCTransportConfig, message::RemoteMedia, peer::API_CHANNEL_LABEL, room::Room,
    },
    track::{
        downtrack::{DownTrack, DownTrackInternal},
        receiver::{Receiver, WebRTCReceiver},
        router::LocalRouter,
    },
};

/// Peer consuming a [RTCPeerConnection] and a [RTCDataChannel]
#[derive(Clone, Default)]
pub struct CentralPeer {
    api_channel: Arc<Mutex<Option<Arc<RTCDataChannel>>>>,
    candidates: Arc<Mutex<Vec<RTCIceCandidateInit>>>,
    channels: Arc<DashMap<String, Arc<RTCDataChannel>>>,
    config: Arc<WebRTCTransportConfig>,
    closed: Arc<AtomicBool>,
    id: String,
    pub room: Arc<Mutex<Option<Arc<Room>>>>,
    #[allow(dead_code)]
    user_id: String,
    on_ice_candidate_fn: Arc<Mutex<Option<OnICECandidateFn>>>,
    on_ice_connection_state_change: Arc<Mutex<Option<OnICEConnectionStateChangeFn>>>,
    on_offer_fn: Arc<Mutex<Option<OnOfferFn>>>,
    pc: Arc<Mutex<Option<Arc<RTCPeerConnection>>>>,
    remote_answer_pending: Arc<AtomicBool>,
    router: Arc<Mutex<Option<Arc<LocalRouter>>>>,
    downtracks: Arc<DashMap<String, Vec<Arc<DownTrack>>>>,
    negotiation_pending: Arc<AtomicBool>,
}

impl CentralPeer {
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
            receiver.clone(),
            self.config.router.max_packet_track,
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
            None => return Err(Error::ErrNoTransportEstablished),
        };

        // New local track
        let mut down_track = DownTrack::new_track_local(self.id.clone(), down_track_local);
        info!(
            "[Peer {}] add_down_track New local track created {}",
            self.id,
            down_track.id()
        );
        down_track.set_transceiver(transceiver.clone());
        let down_track_arc = Arc::new(down_track);

        let peer_1 = self.clone();
        let down_track_1 = down_track_arc.clone();
        let receiver_1 = receiver.clone();
        down_track_arc
            .register_on_close(Box::new(move || {
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
            .register_on_bind(Box::new(move || {
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
                self.config.router.simulcast.best_quality_first,
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Creates an offer for this peer connection and sets it as the local description.
    async fn create_offer(
        &self,
        options: Option<RTCOfferOptions>,
    ) -> Result<RTCSessionDescription> {
        match &*self.pc.lock().await {
            Some(pc) => {
                let offer = pc.create_offer(options).await?;
                pc.set_local_description(offer.clone()).await?;
                Ok(offer)
            }
            None => Err(Error::ErrNoTransportEstablished),
        }
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
                        warn!("[Peer {}] Peer ICE connection closed. No close handler available", id);
                    },
                    _ => {
                        debug!("[Peer {}] Peer ICE connection state changed to: {}", id, state);
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
        pc.on_data_channel(Box::new(move |data_channel| {
            let id_in = id.clone();
            let peer = peer_1.clone();
            Box::pin(async move {
                let label = data_channel.label();
                if label == API_CHANNEL_LABEL {
                    info!("[Peer {}] API data channel open", id_in);
                    *peer.api_channel.lock().await = Some(data_channel);
                } else {
                    info!("[Peer {}] Data channel `{}` open", id_in, label);
                    peer.add_data_channel(data_channel).await;
                }
            })
        }));
    }

    fn on_track(self: &Arc<Self>, pc: &Arc<RTCPeerConnection>, router: Arc<LocalRouter>) {
        let peer_1 = self.clone();
        pc.on_track(Box::new(move |track, receiver, _| {
            let router_in = router.clone();
            let peer_in = peer_1.clone();
            Box::pin(async move {
                let (r, _) = router_in.add_receiver(receiver, track.clone()).await;
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
                if let Some(dcs) = dt.create_source_description_chunks().await.as_mut() {
                    sds.append(dcs);
                }
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
}

#[async_trait]
impl Peer for CentralPeer {
    async fn answer(&self, sdp: RTCSessionDescription) -> Result<RTCSessionDescription> {
        info!("[Peer {}] Got offer", self.id);
        match &*self.pc.lock().await {
            Some(pc) => {
                if pc.signaling_state() != RTCSignalingState::Stable {
                    return Err(Error::ErrOfferIgnored);
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
            None => Err(Error::ErrNoTransportEstablished),
        }
    }

    async fn clean_up(&self) {
        {
            let pc = self.pc.lock().await.take();
            if let Some(pc) = pc {
                if let Err(err) = pc.close().await {
                    error!("[Peer {}] Closing peer connection failed: {err}", self.id);
                }
            }
        }
    }

    async fn join(self: &Arc<Self>, room: Arc<Room>) -> Result<()> {
        let id = &self.id;
        info!("[{id}] Join to {} requested", room.id);
        *self.room.lock().await = Some(room.clone());

        let rtc_config_clone = RTCConfiguration {
            ice_servers: self.config.configuration.ice_servers.clone(),
            ..Default::default()
        };
        let peer_config = WebRTCTransportConfig {
            configuration: rtc_config_clone,
            setting: self.config.setting.clone(),
            router: self.config.router.clone(),
            factory: Arc::default(),
            version: self.config.version.clone(),
        };
        let router = Arc::new(LocalRouter::new(
            self.id.clone(),
            room.clone(),
            self.config.router.clone(),
        ));
        {
            *self.router.lock().await = Some(router);
            let pc = api::create_central_connection(peer_config.into()).await?;
            self.register_handlers(&pc).await;
            *self.pc.lock().await = Some(pc);
        }

        Ok(())
    }

    async fn negotiate(&self, offer_options: Option<RTCOfferOptions>) -> Result<()> {
        debug!("Start negotiation");
        if self.remote_answer_pending.load(Ordering::Relaxed) {
            self.negotiation_pending.store(true, Ordering::Relaxed);
            debug!("Negotiation set to pending. Reason: Remote answer pending");
            return Ok(());
        }

        let offer = self.create_offer(offer_options).await?;
        self.remote_answer_pending.store(true, Ordering::Relaxed);

        if let Some(on_offer) = &mut *self.on_offer_fn.lock().await {
            if !self.closed.load(Ordering::Relaxed) {
                info!("[Peer {}] Send offer", self.id);
                on_offer(offer).await;
            }
        };

        Ok(())
    }

    async fn set_on_ice_candidate(&self, f: OnICECandidateFn) {
        *self.on_ice_candidate_fn.lock().await = Some(f);
    }

    async fn set_on_ice_connection_state_change(&self, f: OnICEConnectionStateChangeFn) {
        *self.on_ice_connection_state_change.lock().await = Some(f);
    }
}
