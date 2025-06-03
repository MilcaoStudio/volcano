use anyhow::Result;
use subscriber::Subscriber;
use std::{
    fmt::Debug,
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use tokio::sync::Mutex;
use webrtc::{
    ice_transport::{
        ice_candidate::{RTCIceCandidate, RTCIceCandidateInit}, ice_connection_state::RTCIceConnectionState
    },
    peer_connection::{
        configuration::RTCConfiguration, sdp::session_description::RTCSessionDescription, signaling_state::RTCSignalingState
    },
};

use crate::{
    buffer::factory::AtomicFactory,
    rtc::peer::publisher::Publisher,
};


use super::{config::WebRTCTransportConfig, room::Room};
use crate::track::error::Error;

mod api;
mod publisher;
pub mod subscriber;

// Callbacks
pub type OnOfferFn = Box<
    dyn (FnMut(RTCSessionDescription) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>)
        + Send
        + Sync,
>;

pub type OnIceCandidateFn = Box<
    dyn (FnMut(RTCIceCandidateInit, u8) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>)
        + Send
        + Sync,
>;

pub type OnIceConnectionStateChangeFn = Box<
    dyn (FnMut(RTCIceConnectionState) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>)
        + Send
        + Sync,
>;

const PUBLISHER: u8 = 0;
const SUBSCRIBER: u8 = 1;

#[derive(Debug, Default, Deserialize)]
pub struct JoinConfig {
    pub no_publish: bool,
    pub no_subscribe: bool,
    pub no_auto_subscribe: bool,
}

/// Abstraction of a WebRTC peer connection
#[derive(Clone)]
pub struct Peer {
    config: Arc<WebRTCTransportConfig>,
    closed: Arc<AtomicBool>,
    id: String,
    pub room: Arc<Mutex<Option<Arc<Room>>>>,
    subscriber: Arc<Mutex<Option<Arc<Subscriber>>>>,
    user_id: String,
    track_map: Arc<Vec<String>>,
    on_ice_candidate_fn: Arc<Mutex<Option<OnIceCandidateFn>>>,
    on_ice_connection_state_change: Arc<Mutex<Option<OnIceConnectionStateChangeFn>>>,
    on_offer_fn: Arc<Mutex<Option<OnOfferFn>>>,
    publisher: Arc<Mutex<Option<Arc<Publisher>>>>,
    remote_answer_pending: Arc<AtomicBool>,
    negotiation_pending: Arc<AtomicBool>,
}

impl Peer {
    /// Create a new Peer
    pub async fn new(user_id: String, config: Arc<WebRTCTransportConfig>) -> Result<Self> {
        // Create track map
        let track_map = Default::default();

        // Construct new Peer
        let peer = Self {
            config,
            closed: Arc::default(),
            id: user_id.clone(),
            room: Arc::default(),
            user_id,
            track_map,
            on_ice_candidate_fn: Arc::default(),
            on_ice_connection_state_change: Arc::default(),
            on_offer_fn: Arc::default(),
            publisher: Arc::default(),
            subscriber: Arc::new(Mutex::new(None)),
            negotiation_pending: Arc::default(),
            remote_answer_pending: Arc::default(),
        };
        
        Ok(peer)
    }

    pub async fn answer(&self, sdp: RTCSessionDescription) -> Result<RTCSessionDescription> {
        if let Some(publisher) = &*self.publisher.lock().await {
            info!("[Peer {}] Get offer", self.id());
            if publisher.signaling_state() != RTCSignalingState::Stable {
                return Err(Error::ErrOfferIgnored.into());
            }
            
            info!("[Publisher {}] Send answer", self.id());
            publisher.answer(sdp).await
        } else {
            Err(Error::ErrNoTransportEstablished.into())
        }
    }
    /// Clean up any open connections
    pub async fn clean_up(&self) {
        // Takes out mutex peers
        let subscriber = self.subscriber.lock().await.take();
        let publisher = self.publisher.lock().await.take();
        if let Some(s) = subscriber {
            s.close().await;
        }

        if let Some(p) = publisher {
            p.close().await;
        }
    }


    pub fn id(&self) -> String {
        self.id.clone()
    }

    pub async fn join(self: &Arc<Self>, room_id: String, cfg: JoinConfig) -> Result<()> {
        let id = &self.id;
        info!("[{id}] Join to {room_id} requested");
        
        let room = Room::get(&room_id);
        *self.room.lock().await = Some(room.clone());
        let rtc_config_clone = RTCConfiguration {
            ice_servers: self.config.configuration.ice_servers.clone(),
            ..Default::default()
        };
        let config = WebRTCTransportConfig {
            configuration: rtc_config_clone,
            setting: self.config.setting.clone(),
            router: self.config.router.clone(),
            factory: Arc::new(Mutex::new(AtomicFactory::new(1000, 1000))),
        };

        if !cfg.no_subscribe {
            let mut inner_subscriber =
                Subscriber::new(self.user_id.clone(), self.config.clone()).await?;
            inner_subscriber.no_auto_subscribe = cfg.no_auto_subscribe;
            let subscriber = Arc::new(inner_subscriber);
            let remote_answer_pending_out = self.remote_answer_pending.clone();
            let negotiation_pending_out = self.negotiation_pending.clone();
            let closed_out = self.closed.clone();
            let sub = Arc::clone(&subscriber);
            let on_offer_handler_out = self.on_offer_fn.clone();
            let id_clone_out = id.clone();
            subscriber
                .register_on_negociate(Box::new(move || {
                    let remote_answer_pending_in = remote_answer_pending_out.clone();
                    let negotiation_pending_in = negotiation_pending_out.clone();
                    let closed_in = closed_out.clone();
                    let id_clone_in = id_clone_out.clone();
                    let sub_in = sub.clone();
                    let on_offer_handler_in = on_offer_handler_out.clone();
                    Box::pin(async move {
                        if remote_answer_pending_in.load(Ordering::Relaxed) {
                            (*negotiation_pending_in).store(true, Ordering::Relaxed);
                            return Ok(());
                        }

                        let offer = sub_in.create_offer().await?;
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
                                on_ice_candidate(val, SUBSCRIBER).await;
                            }
                        }
                    }
                })
            }));
            //let room_out_1 = room.clone();
            //let user_id_out = self.user_id.clone();
            
            *self.subscriber.lock().await = Some(subscriber);
        }

        if !cfg.no_publish {
            if !cfg.no_subscribe {
                for dc in &*room.get_data_channel_middlewares() {
                    if let Some(sub) = &*self.subscriber.lock().await {
                        sub.add_data_channel(&dc.config.label).await?;
                        info!("[Subscriber {}] Trying to offer...", sub.id);
                        sub.create_offer().await?;
                    }
                }
            }
            let on_ice_candidate_out = self.on_ice_candidate_fn.clone();
            let closed_out_1 = self.closed.clone();

            let publisher =
                Arc::new(Publisher::new(self.user_id.clone(), room.clone(), config).await?);

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
                                on_ice_candidate(val, PUBLISHER).await;
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
        info!(
            "[Peer {}] Adds to room {}",
            id, room_id
        );
        
        // Send user join event with no tracks
        room.join_user(id.to_owned(), Vec::new()).await;

        if !cfg.no_subscribe {
            room.subscribe(self.clone()).await;
        }

        Ok(())
    }
    pub async fn publisher(&self) -> Option<Arc<Publisher>> {
        self.publisher.lock().await.clone()
    }

    pub async fn register_on_ice_connection_state_change(&self, f: OnIceConnectionStateChangeFn) {
        let mut handler = self.on_ice_connection_state_change.lock().await;
        *handler = Some(f);
    }
    pub async fn on_ice_candidate(&self, f: OnIceCandidateFn) {
        let mut handler = self.on_ice_candidate_fn.lock().await;
        *handler = Some(f);
    }

    pub async fn on_offer(&self, f: OnOfferFn) {
        let mut handler = self.on_offer_fn.lock().await;
        *handler = Some(f);
    }

    pub async fn set_remote_description(&self, sdp: RTCSessionDescription) -> Result<()> {
        if let Some(subscriber) = &*self.subscriber.lock().await {
            info!("[Peer {}] sets remote description", self.id);
            subscriber.set_remote_description(sdp).await?;
            self.remote_answer_pending.store(false, Ordering::Relaxed);

            if self.negotiation_pending.load(Ordering::Relaxed) {
                self.negotiation_pending.store(false, Ordering::Relaxed);
                info!("Subscriber negotiate");
                subscriber.negotiate().await?;
            }
        } else {
            return Err(Error::ErrNoTransportEstablished.into());
        }

        Ok(())
    }

    pub async fn subscriber(&self) -> Option<Arc<Subscriber>> {
        self.subscriber.lock().await.clone()
    }

    pub async fn trickle(&self, candidate: RTCIceCandidateInit, target: u8) -> Result<()> {
        let subscriber = self.subscriber.lock().await;
        let publisher = self.publisher.lock().await;
        if subscriber.is_none() || publisher.is_none() {
            return Err(Error::ErrNoTransportEstablished.into());
        }

        info!("PeerLocal {} adds ICE candidate", self.id);
        match target {
            PUBLISHER => {
                if let Some(publisher) = &*publisher {
                    publisher.add_ice_candidate(candidate).await?;
                }
            }
            SUBSCRIBER => {
                if let Some(subscriber) = &*subscriber {
                    subscriber.add_ice_candidate(candidate).await?;
                }
            }
            _ => {}
        }
        Ok(())
    }
}

impl Debug for Peer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Peer")
            .field("config", &self.config.router)
            .field("id", &self.id)
            .field("room", &self.room)
            .field("user_id", &self.user_id)
            .field("track_map", &self.track_map)
            .finish()
    }
}
