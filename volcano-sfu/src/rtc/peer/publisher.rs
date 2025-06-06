use std::pin::Pin;
use std::future::Future;
use std::sync::Arc;
use anyhow::Result;
use tokio::sync::Mutex;
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
use webrtc::ice_transport::ice_connection_state::RTCIceConnectionState;
use webrtc::ice_transport::ice_gatherer::OnLocalCandidateHdlrFn;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtcp::packet::Packet as RtcpPacket;
use webrtc::rtp_transceiver::rtp_receiver::RTCRtpReceiver;
use webrtc::rtp_transceiver::RTCRtpTransceiver;
use webrtc::track::track_remote::TrackRemote;

use crate::rtc::config::WebRTCTransportConfig;
use crate::rtc::room::Room;
use crate::track::receiver::Receiver;
use crate::track::router::LocalRouter;

use super::api;

pub type OnIceConnectionStateChange = Box<
    dyn (FnMut(RTCIceConnectionState) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>)
        + Send
        + Sync,
>;


pub struct Publisher {
    id: String,

    pc: Arc<RTCPeerConnection>,

    router: Arc<LocalRouter>,
    room: Arc<Room>,
    tracks: Arc<Mutex<Vec<PublisherTrack>>>,
    candidates: Arc<Mutex<Vec<RTCIceCandidateInit>>>,

    ice_connection_state_change_handler: Arc<Mutex<Option<OnIceConnectionStateChange>>>,
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
        room: Arc<Room>,
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
        let mut publisher = Publisher {
            id: id.clone(),
            pc,
            tracks: Arc::new(Mutex::new(Vec::new())),
            router: Arc::new(LocalRouter::new(id, room.clone(),  router)),
            room,
            candidates: Arc::new(Mutex::new(Vec::new())),
            ice_connection_state_change_handler: Arc::default(),
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
        self.pc.set_remote_description(offer).await?;
        info!("Remote description set on publisher peer connection");

        for c in &*self.candidates.lock().await {
            if let Err(err) = self.pc.add_ice_candidate(c.clone()).await {
                error!("expected answer error :{}", err);
            }
        }

        let answer = self.pc.create_answer(None).await?;
        self.pc.set_local_description(answer.clone()).await?;

        Ok(answer)
    }
    
    pub async fn close(&self) {
        self.router.stop().await;
        if let Err(err) = self.pc.close().await {
            error!("close err: {}", err);
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

    pub async fn on_ice_connection_state_change(&self, f: OnIceConnectionStateChange) {
        let mut handler = self.ice_connection_state_change_handler.lock().await;
        *handler = Some(f);
    }

    async fn on_track(&mut self) {
        let router_out = Arc::clone(&self.router);
        let router_out_2 = Arc::clone(&self.router);
        let room_out = Arc::clone(&self.room);
        let room_out_2 = Arc::clone(&self.room);
        let tracks_out = Arc::clone(&self.tracks);
        let peer_id_out_2 = self.id.clone();
        let pc_out = self.pc.clone();
        let user_id_out = self.id.clone();

        self.pc.on_track(Box::new(
            move |track: Arc<TrackRemote>, receiver: Arc<RTCRtpReceiver>, _: Arc<RTCRtpTransceiver>| {
                let router_in = Arc::clone(&router_out);
                let router_in2 = Arc::clone(&router_out);
                let room_in = Arc::clone(&room_out);
                let tracks_in = Arc::clone(&tracks_out);
                let user_id_in = user_id_out.clone();

                Box::pin(async move {
                    let track_id = track.id();
                    let track_stream_id = track.stream_id();
                    let track_clone = track.clone();
                    info!("Track {} from stream {} received", track_id, track_stream_id);

                    let (r, publish) = router_in
                        .add_receiver(receiver, track_clone.clone(), track_id, track_stream_id)
                        .await;
                    debug!("Add track receiver with track {} into router", r.track_id());
                    let receiver_clone = r.clone();
                    
                    if publish {
                        room_in.publish_track(router_in2, r.clone()).await;
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
                    let tracks: Vec<String> = tracks_in.lock().await.iter().map(|t: &PublisherTrack| t.track.id()).collect();
                    room_in.join_user(user_id_in, tracks).await;
                })
            })
        );

        self.pc.on_data_channel(Box::new(move |channel| {
            let room_in = Arc::clone(&room_out_2);
            let id_in = peer_id_out_2.clone();
            // Ignore our default channel, exists to force ICE candidates. See signalPair for more info
            if channel.label() == super::subscriber::API_CHANNEL_LABEL {
                info!("[Publisher {id_in}] API data channel published from client!");
                return Box::pin(async move {
                    //room_in.add_api_channel(&id_in).await;
                });
            }
            Box::pin(async move {
                room_in.add_data_channel(&id_in, channel).await;
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

    pub fn signaling_state(&self) -> webrtc::peer_connection::signaling_state::RTCSignalingState {
        self.pc.signaling_state()
    }
}