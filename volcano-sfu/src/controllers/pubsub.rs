use std::{
    future::Future, pin::Pin, sync::{
        atomic::{AtomicBool, Ordering}, Arc
    }
};

use async_trait::async_trait;
use serde_repr::{Deserialize_repr, Serialize_repr};
use tokio::sync::Mutex;
use webrtc::{
    data_channel::RTCDataChannel,
    ice_transport::{
        ice_candidate::{RTCIceCandidate, RTCIceCandidateInit},
        ice_connection_state::RTCIceConnectionState,
    },
    peer_connection::{
        OnDataChannelHdlrFn, configuration::RTCConfiguration,
        offer_answer_options::RTCOfferOptions, sdp::session_description::RTCSessionDescription,
    },
};

use super::{PeerConfig, PeerController, PeerControllerError, Result};
use crate::{
    peer::{Consumer, OnICEConnectionStateChangeFn, OnOfferFn, Publisher, Subscriber},
    session::{config::WebRTCTransportConfig, room::Room}, track::router::LocalRouter,
};

#[derive(Clone, Copy, Debug, Serialize_repr, Deserialize_repr)]
#[repr(u8)]
pub enum PeerRole {
    Publisher,
    Subscriber,
}

pub type OnPubSubICECandidateFn = Box<
    dyn (FnMut(RTCIceCandidateInit, PeerRole) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>)
        + Send
        + Sync,
>;

/// Two peer connections are used by this peer. Ideal for handling multiple requests.
///
/// - [Publisher] peer listens for client's remote media tracks and publishes them into the publisher's router.
/// - [Subscriber] peer subscribes to local tracks and sends them to client's peer connection.
///
/// Suggestion: Despite of this peer implements [Default] trait, it is recommended to use [Self::new] to set an unique ID for this peer.
#[derive(Clone, Default)]
pub struct PubSubController {
    config: Arc<WebRTCTransportConfig>,
    closed: Arc<AtomicBool>,
    id: String,
    pub subscriber: Arc<Mutex<Option<Arc<Subscriber>>>>,
    on_data_channel_fn: Arc<Mutex<Option<OnDataChannelHdlrFn>>>,
    on_ice_candidate_fn: Arc<Mutex<Option<OnPubSubICECandidateFn>>>,
    on_ice_connection_state_change: Arc<Mutex<Option<OnICEConnectionStateChangeFn>>>,
    on_offer_fn: Arc<Mutex<Option<OnOfferFn>>>,
    pub publisher: Arc<Mutex<Option<Arc<Publisher>>>>,
    user_id: String,
}

impl PubSubController {

    /// Creates a new Peer
    pub fn new(id: String, user_id: String, config: Arc<WebRTCTransportConfig>) -> Self {
        Self {
            config,
            user_id,
            id,
            ..Default::default()
        }
    }

    pub async fn join(&self, room: Arc<Room>, cfg: &PeerConfig) -> Result<()> {
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

            let on_data_channel_out = self.on_data_channel_fn.clone();
            publisher
                .on_data_channel(Box::new(move |dc| {
                    let handler_in = on_data_channel_out.clone();
                    Box::pin(async move {
                        if let Some(handler) = handler_in.lock().await.as_mut() {
                            handler(dc).await;
                        }
                    })
                }))
                .await;

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

        //room.add_peer(self.clone()).await;
        info!("[Peer {}] Adds to room {}", id, room.id);
        room.add_user(id.to_owned());

        Ok(())
    }

    pub async fn on_ice_candidate(&self, f: OnPubSubICECandidateFn) {
        let mut handler = self.on_ice_candidate_fn.lock().await;
        *handler = Some(f);
    }

    pub async fn setup_subscriber(&self, cfg: &PeerConfig) -> Result<()> {
        let mut inner_subscriber =
            Subscriber::new(self.user_id.clone(), self.config.clone()).await?;
        inner_subscriber.no_auto_subscribe = cfg.no_auto_subscribe;
        let subscriber = Arc::new(inner_subscriber);

        let on_offer_handler_out = self.on_offer_fn.clone();
        subscriber
            .on_offer(Box::new(move |offer| {
                let offer_handler_in = on_offer_handler_out.clone();
                Box::pin(async move {
                    if let Some(handler) = offer_handler_in.lock().await.as_mut() {
                        handler(offer).await;
                    }
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

    pub async fn trickle(&self, candidate: RTCIceCandidateInit, target: PeerRole) -> Result<()> {
        info!("PubSub {} adds ICE candidate to {target:?}", self.id);
        match target {
            PeerRole::Publisher => match self.publisher.lock().await.as_ref() {
                Some(publisher) => publisher
                    .add_ice_candidate(candidate)
                    .await
                    .map_err(Into::into),
                _ => Err(PeerControllerError::ErrNoProducer),
            },
            PeerRole::Subscriber => match self.subscriber.lock().await.as_ref() {
                Some(subscriber) => subscriber
                    .add_ice_candidate(candidate)
                    .await
                    .map_err(Into::into),
                _ => Err(PeerControllerError::ErrNoConsumer),
            },
        }
    }

    pub async fn register_on_ice_connection_state_change(&self, f: OnICEConnectionStateChangeFn) {
        let mut handler = self.on_ice_connection_state_change.lock().await;
        *handler = Some(f);
    }

    pub async fn on_offer(&self, f: OnOfferFn) {
        let mut handler = self.on_offer_fn.lock().await;
        *handler = Some(f);
    }

    pub async fn subscriber(&self) -> Option<Arc<Subscriber>> {
        self.subscriber.lock().await.clone()
    }
}

#[async_trait]
impl PeerController for PubSubController {
    async fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);

        if let Some(s) = self.subscriber.lock().await.take() {
            s.close().await;
        }

        if let Some(p) = self.publisher.lock().await.take() {
            p.close().await;
        }
    }

    async fn close_local_data_channel(&self, label: &str) -> Result<()> {
        if let Some(subscriber) = self.subscriber.lock().await.as_ref() {
            let dc = match subscriber.channels.remove(label) {
                Some((_, dc)) => dc,
                _ => return Ok(())
            };
            dc.close().await?;
        }
        Ok(())
    }

    async fn consumer(&self) -> Option<Arc<dyn Consumer + Send + Sync>> {
        match self.subscriber.lock().await.as_ref() {
            Some(subscriber) => Some(subscriber.clone()),
            None => None,
        }
    }

    async fn create_local_data_channel(&self, label: String) -> Result<Arc<RTCDataChannel>> {
        if let Some(subscriber) = self.subscriber.lock().await.as_ref() {
            subscriber
                .create_data_channel(label)
                .await
                .map_err(Into::into)
        } else {
            Err(PeerControllerError::ErrNoConsumer)
        }
    }

    fn id(&self) -> String {
        self.id.clone()
    }

    async fn local_data_channel(&self, label: &str) -> Option<Arc<RTCDataChannel>> {
        match self.subscriber.lock().await.as_ref() {
            Some(sub) => sub.data_channel(label).await,
            None => None,
        }
    }

    async fn negotiate(&self, offer_options: Option<RTCOfferOptions>) -> Result<()> {
        // Negotiate on subscriber (it creates offers)
        if let Some(subscriber) = self.subscriber.lock().await.as_ref() {
            subscriber
                .negotiate(offer_options)
                .await
                .map_err(Into::into)
        } else {
            Err(PeerControllerError::ErrNoConsumer)
        }
    }

    async fn on_data_channel(&self, f: OnDataChannelHdlrFn) {
        let mut handler = self.on_data_channel_fn.lock().await;
        *handler = Some(f);
    }

    async fn on_remote_answer(&self, sdp: RTCSessionDescription) -> Result<()> {
        // This delegates to subscriber's on_remote_answer and ends the SDP exchange
        match self.subscriber.lock().await.as_ref() {
            Some(sub) => sub.on_remote_answer(sdp).await.map_err(Into::into),
            _ => Err(PeerControllerError::ErrNoConsumer),
        }
    }

    async fn on_remote_offer(&self, sdp: RTCSessionDescription) -> Result<RTCSessionDescription> {
        match &*self.publisher.lock().await {
            Some(publisher) => {
                info!("[Peer {}] Got offer", self.id);
                publisher.on_remote_offer(sdp).await.map_err(Into::into)
            }
            _ => Err(PeerControllerError::ErrNoProducer),
        }
    }

    async fn on_ice_connection_state_change(&self, f: OnICEConnectionStateChangeFn) {
        self.register_on_ice_connection_state_change(f).await;
    }

    async fn router(&self) -> Result<Arc<LocalRouter>> {
        match self.publisher.lock().await.as_ref() {
            Some(publisher) => Ok(publisher.router()),
            _ => Err(PeerControllerError::ErrNoProducer),
        }
    }
}