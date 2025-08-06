use std::{
    fmt::Debug,
    sync::{
        atomic::{AtomicBool, Ordering}, Arc
    },
};
use tokio::sync::Mutex;
use webrtc::{
    ice_transport::{
        ice_candidate::{RTCIceCandidate, RTCIceCandidateInit},
        ice_connection_state::RTCIceConnectionState,
    },
    peer_connection::{
        configuration::RTCConfiguration, offer_answer_options::RTCOfferOptions,
        sdp::session_description::RTCSessionDescription, signaling_state::RTCSignalingState,
    },
};

use super::{
    OnICEConnectionStateChangeFn, OnOfferFn, OnPubSubICECandidateFn, PeerConfig, Publisher,
    Subscriber, Result, Error
};
use crate::rtc::{config::WebRTCTransportConfig, peer::PeerRole, room::Room};

/// Two peer connections are used by this peer. Ideal for handling multiple requests.
///
/// - [Publisher] peer listens for client's remote media tracks and publishes them into the publisher's router.
/// - [Subscriber] peer subscribes to local tracks and sends them to client's peer connection.
///
/// Suggestion: Despite of this peer implements [Default] trait, it is recommended to use [Self::new] to set an unique ID for this peer.
#[derive(Default)]
pub struct PubSubPeer {
    config: Arc<WebRTCTransportConfig>,
    closed: Arc<AtomicBool>,
    id: String,
    subscriber: Mutex<Option<Arc<Subscriber>>>,
    user_id: String,
    track_map: Arc<Vec<String>>,
    on_ice_candidate_fn: Arc<Mutex<Option<OnPubSubICECandidateFn>>>,
    on_ice_connection_state_change: Arc<Mutex<Option<OnICEConnectionStateChangeFn>>>,
    on_offer_fn: Arc<Mutex<Option<OnOfferFn>>>,
    publisher: Mutex<Option<Arc<Publisher>>>,
    remote_answer_pending: Arc<AtomicBool>,
    negotiation_pending: Arc<AtomicBool>,
}

impl PubSubPeer {
    /// Creates a new Peer
    pub fn new(id: String, config: Arc<WebRTCTransportConfig>) -> Self {
        Self {
            config,
            user_id: id.clone(),
            id,
            ..Default::default()
        }
    }

    pub async fn answer(&self, sdp: RTCSessionDescription) -> Result<RTCSessionDescription> {
        match &*self.publisher.lock().await {
            Some(publisher) => {
                info!("[Peer {}] Get offer", self.id());
                if publisher.signaling_state() != RTCSignalingState::Stable {
                    return Err(Error::ErrOfferIgnored);
                }

                info!("[Publisher {}] Send answer", self.id());
                publisher.answer(sdp).await.map_err(Into::into)
            }
            _ => Err(Error::ErrNoTransportEstablished),
        }
    }
    /// Clean up any open connections
    pub async fn clean_up(&self) {
        // Takes out mutex peers
        {
            let subscriber = self.subscriber.lock().await.take();
            if let Some(s) = subscriber {
                s.close().await;
            }
        }

        {
            let publisher = self.publisher.lock().await.take();
            if let Some(p) = publisher {
                p.close().await;
            }
        }
    }

    pub fn config(&self) -> Arc<WebRTCTransportConfig> {
        self.config.clone()
    }

    pub fn id(&self) -> String {
        self.id.clone()
    }

    pub async fn join(self: &Arc<Self>, room: Arc<Room>, cfg: &PeerConfig) -> Result<()> {
        let id = &self.id;
        info!("[{id}] Join to {} requested", room.id);

        let weak_room = Arc::downgrade(&room);
        let mut peer_config = (*self.config).clone();
        {
            peer_config.configuration = RTCConfiguration {
                ice_servers: peer_config.ice_servers.clone(),
                ..Default::default()
            };
            peer_config.factory = Arc::default();
        }
        
        if !cfg.no_publish {
            let on_ice_candidate_out = self.on_ice_candidate_fn.clone();
            let closed_out_1 = self.closed.clone();

            let publisher =
                Arc::new(Publisher::new(self.user_id.clone(), weak_room, peer_config).await?);

            publisher.on_ice_candidate(Box::new(move |candidate: Option<RTCIceCandidate>| {
                let on_ice_candidate_in = on_ice_candidate_out.clone();
                let closed_in = closed_out_1.clone();
                Box::pin(async move {
                    if candidate.is_none() {
                        return;
                    }

                    if let Some(on_ice_candidate) = &mut *on_ice_candidate_in.lock().await {
                        if !closed_in.load(Ordering::Relaxed) {
                            if let Ok(val) = candidate.unwrap().to_json() {
                                on_ice_candidate(val, PeerRole::Publisher).await;
                            }
                        }
                    }
                })
            }));

            let on_ice_connection_state_change_out = self.on_ice_connection_state_change.clone();
            let closed_out_2 = self.closed.clone();

            publisher
                .on_ice_connection_state_change(Box::new(move |state: RTCIceConnectionState| {
                    let handler_in = on_ice_connection_state_change_out.clone();
                    let closed_in = closed_out_2.clone();

                    Box::pin(async move {
                        if let Some(h) = &mut *handler_in.lock().await {
                            if !closed_in.load(Ordering::Relaxed) {
                                h(state).await;
                            }
                        }
                    })
                }))
                .await;

            *self.publisher.lock().await = Some(publisher);
        }

        room.add_peer(self.clone()).await;
        info!("[Peer {}] Adds to room {}", id, room.id);
        room.add_user(id.to_owned());

        Ok(())
    }
    pub async fn publisher(&self) -> Option<Arc<Publisher>> {
        self.publisher.lock().await.clone()
    }

    pub async fn register_on_ice_connection_state_change(&self, f: OnICEConnectionStateChangeFn) {
        let mut handler = self.on_ice_connection_state_change.lock().await;
        *handler = Some(f);
    }

    pub async fn setup_subscriber(&self, cfg: &PeerConfig) -> Result<()> {
        let mut inner_subscriber =
            Subscriber::new(self.user_id.clone(), self.config.clone()).await?;
        inner_subscriber.no_auto_subscribe = cfg.no_auto_subscribe;
        let subscriber = Arc::new(inner_subscriber);

        let remote_answer_pending_out = self.remote_answer_pending.clone();
        let negotiation_pending_out = self.negotiation_pending.clone();
        let closed_out = self.closed.clone();
        let sub = Arc::clone(&subscriber);
        let on_offer_handler_out = self.on_offer_fn.clone();
        let id_clone_out = self.id.clone();
        subscriber
            .register_on_negotiate(Box::new(move |offer_options: Option<RTCOfferOptions>| {
                let remote_answer_pending_in = remote_answer_pending_out.clone();
                let negotiation_pending_in = negotiation_pending_out.clone();
                let closed_in = closed_out.clone();
                let id_clone_in = id_clone_out.clone();
                let sub_in = sub.clone();
                let on_offer_handler_in = on_offer_handler_out.clone();
                Box::pin(async move {
                    debug!("Start negotiation");
                    if remote_answer_pending_in.load(Ordering::Relaxed) {
                        (*negotiation_pending_in).store(true, Ordering::Relaxed);
                        debug!("Negotiation set to pending. Reason: Remote answer pending");
                        return Ok(());
                    }

                    let offer = sub_in.create_offer(offer_options).await?;
                    (*remote_answer_pending_in).store(true, Ordering::Relaxed);

                    if let Some(on_offer) = &mut *on_offer_handler_in.lock().await {
                        if !closed_in.load(Ordering::Relaxed) {
                            info!("[Peer {}] Send offer", id_clone_in);
                            on_offer(offer).await;
                        }
                    }

                    Ok(())
                })
            }))
            .await;
        let on_ice_candidate_out = self.on_ice_candidate_fn.clone();
        let closed_out_ = self.closed.clone();
        subscriber.register_on_ice_candidate(Box::new(move |candidate| {
            let on_ice_candidate_in = on_ice_candidate_out.clone();
            let closed_in = closed_out_.clone();
            Box::pin(async move {
                if candidate.is_none() {
                    return;
                }
                if let Some(on_ice_candidate) = &mut *on_ice_candidate_in.lock().await {
                    if !closed_in.load(Ordering::Relaxed) {
                        if let Ok(val) = candidate.unwrap().to_json() {
                            on_ice_candidate(val, PeerRole::Subscriber).await;
                        }
                    }
                }
            })
        }));

        *self.subscriber.lock().await = Some(subscriber);
        Ok(())
    }

    pub async fn on_ice_candidate(&self, f: OnPubSubICECandidateFn) {
        let mut handler = self.on_ice_candidate_fn.lock().await;
        *handler = Some(f);
    }

    pub async fn on_offer(&self, f: OnOfferFn) {
        let mut handler = self.on_offer_fn.lock().await;
        *handler = Some(f);
    }

    pub async fn set_remote_description(&self, sdp: RTCSessionDescription) -> Result<()> {
        match &*self.subscriber.lock().await {
            Some(subscriber) => {
                info!("[Peer {}] sets remote description", self.id);
                subscriber.set_remote_description(sdp).await?;
                self.remote_answer_pending.store(false, Ordering::Relaxed);

                if self.negotiation_pending.swap(false, Ordering::Relaxed) {
                    info!("Negotiation pending. Start new negotiation.");
                    subscriber.negotiate(None).await?;
                }

                subscriber.on_answer().map_err(Into::into)
            }
            _ => Err(Error::ErrNoTransportEstablished),
        }
    }

    pub async fn subscriber(&self) -> Option<Arc<Subscriber>> {
        self.subscriber.lock().await.clone()
    }

    pub async fn trickle(&self, candidate: RTCIceCandidateInit, target: PeerRole) -> Result<()> {
        info!("PubSub {} adds ICE candidate to {target:?}", self.id);
        match target {
            PeerRole::Publisher => {
                match self.publisher.lock().await.as_ref() {
                    Some(publisher) => publisher.add_ice_candidate(candidate).await.map_err(Into::into),
                    _ => Err(Error::ErrNoPublisher)
                }
            }
            PeerRole::Subscriber => {
                match self.subscriber.lock().await.as_ref() {
                    Some(subscriber) => subscriber.add_ice_candidate(candidate).await.map_err(Into::into),
                    _ => Err(Error::ErrNoSubscriber)
                }
            }
        }
    }
}

impl Debug for PubSubPeer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Peer")
            .field("config", &self.config.router)
            .field("id", &self.id)
            .field("user_id", &self.user_id)
            .field("track_map", &self.track_map)
            .finish()
    }
}
