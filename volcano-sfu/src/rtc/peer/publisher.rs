use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use anyhow::Result;
use tokio::sync::Mutex;
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
use webrtc::ice_transport::ice_connection_state::RTCIceConnectionState;
use webrtc::ice_transport::ice_gatherer::OnLocalCandidateHdlrFn;
use webrtc::peer_connection::{RTCPeerConnection, sdp::session_description::RTCSessionDescription};
use webrtc::rtcp::packet::Packet as RtcpPacket;
use webrtc::rtp_transceiver::rtp_receiver::RTCRtpReceiver;
use webrtc::rtp_transceiver::RTCRtpTransceiver;
use webrtc::track::track_remote::TrackRemote;

use crate::rtc::config::WebRTCTransportConfig;
use crate::rtc::room::{Room};
use crate::track::receiver::Receiver;
use crate::track::router::LocalRouter;

use super::{api, OnICEConnectionStateChangeFn};

pub struct Publisher {
    id: String,
    
    pc: Arc<RTCPeerConnection>,

    router: Arc<LocalRouter>,
    room: Weak<Room>,
    tracks: Arc<Mutex<Vec<PublisherTrack>>>,
    candidates: Arc<Mutex<Vec<RTCIceCandidateInit>>>,
    session_version: AtomicU64,

    ice_connection_state_change_handler: Arc<Mutex<Option<OnICEConnectionStateChangeFn>>>,
}

#[derive(Clone)]
pub(super) struct PublisherTrack {
    track: Arc<TrackRemote>,
    #[allow(dead_code)]
    receiver: Arc<dyn Receiver>,

    // This will be used in the future for tracks that will be relayed as clients or servers
    // This is for SVC and Simulcast where you will be able to chose if the relayed peer just
    // want a single track (for recording/ processing) or get all the tracks (for load balancing)
    #[allow(dead_code)]
    client_relay: bool,
}

impl Publisher {
    pub async fn new(
        id: String,
        room: Weak<Room>,
        cfg: WebRTCTransportConfig,
    ) -> Result<Self> {
        let router = cfg.router.clone();
        /*
        let rtc_config = RTCConfiguration {
            ice_servers: cfg.configuration.ice_servers.clone(),
            ..Default::default()
        };

        
        let config_clone = WebRTCTransportConfig {
            configuration: rtc_config,
            setting: cfg.setting.clone(),
            router: cfg.router.clone(),
            factory: Arc::new(Mutex::new(AtomicFactory::new(1000, 1000))),
        };rtc_confrtc_configig
        */

        let pc = api::create_publisher_connection(cfg).await?;
        let publisher = Publisher {
            id: id.clone(),
            pc,
            tracks: Arc::new(Mutex::new(Vec::new())),
            router: Arc::new(LocalRouter::new(id, room.clone(),  router)),
            room,
            candidates: Arc::new(Mutex::new(Vec::new())),
            ice_connection_state_change_handler: Arc::default(),
            session_version: AtomicU64::default(),
        };

        publisher.on_track().await;

        Ok(publisher)
    }

    pub async fn add_ice_candidate(&self, candidate: RTCIceCandidateInit) -> Result<()> {
        if self.pc.remote_description().await.is_some() {
            self.pc.add_ice_candidate(candidate.clone()).await?;
            info!("publisher::add_ice_candidate add candidate into peer connection");
            return Ok(());
        }

        info!("publisher::add_ice_candidate add candidate into candidates vector");
        self.candidates.lock().await.push(candidate.clone());

        Ok(())
    }

    pub async fn answer(&self, offer: RTCSessionDescription) -> Result<RTCSessionDescription> {
        let mut candidates = self.candidates.lock().await;

        if let Some(current_session) = self.pc.current_remote_description().await
            .and_then(|desc| desc.unmarshal().ok()) {
                if let Some(new_session) = offer.unmarshal().ok() {
                    if new_session.origin.session_version > current_session.origin.session_version {
                        candidates.clear();
                        debug!("This offer contains a new session version. Candidates are cleaned up.");
                    }
                }
                self.session_version.store(current_session.origin.session_version, Ordering::Relaxed);
        }

        self.pc.set_remote_description(offer).await?;
        for c in &*candidates {
            if let Err(err) = self.pc.add_ice_candidate(c.clone()).await {
                warn!("[Publisher {}] Add candidate into peer connection failed: {}", self.id, err);
            }
        }

        let answer = self.pc.create_answer(None).await?;
        self.pc.set_local_description(answer.clone()).await?;

        Ok(answer)
    }
    
    pub async fn close(&self) {
        self.router.stop().await;

        // Remove ice connection state change handler
        *self.ice_connection_state_change_handler.lock().await = None;
        match self.pc.close().await {
            Ok(_) => {
                info!("[Publisher {}] Peer connection closed", self.id);
            },
            Err(err) => {
                warn!("[Publisher {}] Peer connection close failed: {err}", self.id);
            }
        }
    }

    async fn close_publisher(router: Arc<LocalRouter>, pc: Arc<RTCPeerConnection>) {
        router.stop().await;
        if let Err(err) = pc.close().await {
            error!("close err: {}", err);
        }
    }

    pub async fn get_tracks(&self) -> Vec<Arc<TrackRemote>> {
        let tracks = &*self.tracks.lock().await;
        tracks.iter().map(|t| t.track.clone()).collect()
    }

    pub fn on_ice_candidate(&self, f: OnLocalCandidateHdlrFn) {
        self.pc.on_ice_candidate(f);
    }

    pub async fn on_ice_connection_state_change(&self, f: OnICEConnectionStateChangeFn) {
        let mut handler = self.ice_connection_state_change_handler.lock().await;
        *handler = Some(f);
    }

    async fn on_track(&self) {
        let router_out = Arc::clone(&self.router);
        let router_out_2 = Arc::clone(&self.router);
        let room_out = self.room.clone();
        let room_out_2 = self.room.clone();
        let tracks_out = Arc::clone(&self.tracks);
        let peer_id_out_2 = self.id.clone();
        let user_id_out = self.id.clone();
        let pc_out = self.pc.clone();

        self.pc.on_track(Box::new(
            move |track: Arc<TrackRemote>, receiver: Arc<RTCRtpReceiver>, _: Arc<RTCRtpTransceiver>| {
                let router_in = Arc::clone(&router_out);
                let room_in = room_out.clone();
                let tracks_in = Arc::clone(&tracks_out);
                let user_id_in = user_id_out.clone();

                Box::pin(async move {
                    let track_id = track.id();
                    let track_stream_id = track.stream_id();
                    let track_clone = track.clone();
                    info!("Track {} from stream {} received", track_id, track_stream_id);
                    let receiver_2 = receiver.clone();

                    let (r, publish) = router_in
                        .add_receiver(receiver, track_clone.clone(),)
                        .await;
                    debug!("[Publisher {}] Add track receiver with track {} into router", user_id_in, r.track_id());
                    let receiver_clone = r.clone();
                    if publish {
                        if let Some(room) = room_in.upgrade() {
                            room.publish_track(&router_in, r).await;
                        } else {
                            warn!("Publish track failed.");
                        }
                        tracks_in.lock().await.push(PublisherTrack {
                            track: track_clone.clone(),
                            receiver: receiver_clone,
                            client_relay: true,
                        });
                    } else {
                        tracks_in.lock().await.push(PublisherTrack {
                            track: track_clone,
                            receiver: r,
                            client_relay: false,
                        })
                    }
                    let recv_tracks = receiver_2.tracks().await;
                    let tracks = recv_tracks.iter().map(|t| (t.id(), t.rid())).collect::<Vec<_>>();
                    if let Some(room) = room_in.upgrade() {
                        for (track_id, track_rid) in tracks {
                            info!("[Publisher {}] Adding track {} [{}] to user", user_id_in, track_id, track_rid);
                            room.add_user_track(user_id_in.clone(), track_stream_id.clone(), track_id, track_rid.len() > 0);
                        }
                    }
                })
            })
        );

        self.pc.on_data_channel(Box::new(move |channel| {
            let room_in = room_out_2.clone();
            let id_in = peer_id_out_2.clone();
            // Ignore our default channel, exists to force ICE candidates. See signalPair for more info
            if channel.label() == super::API_CHANNEL_LABEL {
                info!("[Publisher {id_in}] API data channel published from client!");
                return Box::pin(async move {
                    //room_in.add_api_channel(&id_in).await;
                });
            }
            Box::pin(async move {
                if let Some(room) = room_in.upgrade() {
                    room.add_data_channel(&id_in, channel).await;
                }
            })
        }));

        let on_ice_connection_state_change_clone = self.ice_connection_state_change_handler.clone();
        self.pc.on_ice_connection_state_change(Box::new(move |s| {
            let router_in = Arc::clone(&router_out_2);
            let pc_in = pc_out.clone();
            let handler_in = Arc::clone(&on_ice_connection_state_change_clone);
            Box::pin(async move {
                if let Some(h) = &mut *handler_in.lock().await {
                    h(s).await;
                }
                match s {
                    RTCIceConnectionState::Failed | RTCIceConnectionState::Closed => {
                        Publisher::close_publisher(router_in, pc_in).await;
                    }
                    _ => {}
                }
            })
        }));

        let pc_clone_out = self.pc.clone();
        self.router.set_rtcp_writer(Box::new(
            move |packets: Vec<Box<dyn RtcpPacket + Send + Sync>>| {
                let pc_clone_in = pc_clone_out.clone();
                Box::pin(async move {
                    pc_clone_in.write_rtcp(&packets[..]).await?;
                    Ok(())
                })
            },
        )).await;

        let router_clone = self.router.clone();
        tokio::spawn(async move {
            router_clone.send_rtcp().await;
        });
    }

    pub fn router(&self) -> Arc<LocalRouter> {
        self.router.clone()
    }

    pub fn session_version(&self) -> u64 {
        self.session_version.load(Ordering::Acquire)
    }

    pub fn signaling_state(&self) -> webrtc::peer_connection::signaling_state::RTCSignalingState {
        self.pc.signaling_state()
    }
}