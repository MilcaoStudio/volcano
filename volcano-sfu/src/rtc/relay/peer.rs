use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::Arc;

use ulid::Ulid;
use webrtc::ice_transport::ice_gatherer::RTCIceGatherOptions;
use webrtc::ice_transport::ice_gatherer_state::RTCIceGathererState;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::noop::NoOp;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpParameters;
use webrtc::rtp_transceiver::rtp_codec::RTPCodecType;
use webrtc::rtp_transceiver::RTCRtpReceiveParameters;
use webrtc::rtp_transceiver::RTCRtpSendParameters;
use webrtc::track::track_remote::TrackRemote;
use std::future::Future;
use tokio::sync::mpsc;
use tokio::sync::Mutex;
use tokio::time::Duration;
use webrtc::data_channel::RTCDataChannel;
use webrtc::dtls_transport::RTCDtlsTransport;
use webrtc::ice_transport::ice_gatherer::RTCIceGatherer;
use webrtc::ice_transport::ice_role::RTCIceRole;
use webrtc::ice_transport::RTCIceTransport;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::setting_engine::SettingEngine;
use webrtc::api::{API, APIBuilder};
use webrtc::dtls_transport::dtls_parameters::DTLSParameters;
use webrtc::ice_transport::ice_candidate::RTCIceCandidate;
use webrtc::ice_transport::ice_parameters::RTCIceParameters;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecParameters;
use webrtc::rtp_transceiver::rtp_receiver::RTCRtpReceiver;
use webrtc::rtp_transceiver::rtp_sender::RTCRtpSender;
use webrtc::rtp_transceiver::RTCRtpCodingParameters;
use webrtc::sctp_transport::sctp_transport_capabilities::SCTPTransportCapabilities;
use webrtc::sctp_transport::RTCSctpTransport;
use webrtc::track::track_local::TrackLocal;

use crate::rtc::room::Room;
use crate::rtc::room::RoomEvent;

use super::error::Error;
use anyhow::Result;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackMeta {
    #[serde(rename = "streamId")]
    stream_id: String,
    #[serde(rename = "trackId")]
    track_id: String,
    //https://serde.rs/field-attrs.html#skip
    #[serde(skip_serializing, skip_deserializing)]
    codec_parameters: Option<RTCRtpCodecParameters>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Signal {
    #[serde(rename = "encodings", skip_serializing_if = "Option::is_none")]
    encodings: Option<RTCRtpCodingParameters>,
    #[serde(rename = "iceCandidates")]
    ice_candidates: Vec<RTCIceCandidate>,
    #[serde(rename = "iceParameters")]
    ice_parameters: RTCIceParameters,
    #[serde(rename = "dtlsParameters")]
    dtls_parameters: DTLSParameters,
    #[serde(rename = "sctpTransportCapabilities")]
    sctp_capabilities: SCTPTransportCapabilities,
    #[serde(rename = "trackInfo", skip_serializing_if = "Option::is_none")]
    track_meta: Option<TrackMeta>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    id: String,
    #[serde(rename = "reply")]
    is_reply: bool,
    event: RoomEvent,
}

pub struct PeerConfig {
    pub setting_engine: SettingEngine,
    pub ice_servers: Vec<RTCIceServer>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerMeta {
    #[serde(rename = "peerId")]
    pub peer_id: String,
    #[serde(rename = "roomId")]
    pub room_id: String,
}
pub type OnPeerReadyFn =
    Box<dyn (FnMut() -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>) + Send + Sync>;
pub type OnPeerCloseFn =
    Box<dyn (FnMut() -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>) + Send + Sync>;

pub type OnPeerDataChannelFn =
    Box<dyn (FnMut(Arc<RTCDataChannel>) -> Pin<Box<dyn Future<Output = ()> + Send>>) + Send + Sync>;

pub type OnPeerTrackFn = Box<
    dyn (FnMut(
            Arc<TrackRemote>,
            Arc<RTCRtpReceiver>,
            TrackMeta,
        ) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>)
        + Send
        + Sync,
>;
pub struct Peer {
    media_engine: Arc<Mutex<MediaEngine>>,
    api: Arc<API>,
    ice_transport: Arc<RTCIceTransport>,
    peer_meta: PeerMeta,
    sctp_transport: Arc<RTCSctpTransport>,
    dtls_transport: Arc<RTCDtlsTransport>,
    ice_role: Arc<RTCIceRole>,
    ready: bool,
    rtp_senders: Vec<Arc<RTCRtpSender>>,
    rtp_receivers: Vec<Arc<RTCRtpReceiver>>,
    pending_requests: Arc<Mutex<BTreeMap<String, mpsc::UnboundedSender<Vec<u8>>>>>,
    local_tracks: Vec<Arc<dyn TrackLocal + Send + Sync>>,
    ice_gatherer: Arc<RTCIceGatherer>,
    on_close_handler: Arc<Mutex<Option<OnPeerCloseFn>>>,
}

pub struct RelayPeer {
    pub peer: Peer,
    data_channels: Vec<Arc<RTCDataChannel>>,
}

impl Peer {
    pub(crate) fn new(meta: PeerMeta, conf: PeerConfig) -> Result<Self> {
        let ice_options = RTCIceGatherOptions {
            ice_servers: conf.ice_servers,
            ..Default::default()
        };

        let me = MediaEngine::default();
        let api = APIBuilder::new()
            .with_media_engine(me)
            .with_setting_engine(conf.setting_engine)
            .build();
        // Create the ICE gatherer
        let gatherer = Arc::new(api.new_ice_gatherer(ice_options)?);
        // Construct the ICE transport
        let ice = Arc::new(api.new_ice_transport(Arc::clone(&gatherer)));
        // Construct the DTLS transport
        let dtls = Arc::new(api.new_dtls_transport(Arc::clone(&ice), vec![])?);
        // Construct the SCTP transport
        let sctp = Arc::new(api.new_sctp_transport(Arc::clone(&dtls))?);

        //let signaling_dc = Arc::new(RTCDataChannel::default());

        Ok(Self {
            media_engine: Arc::new(Mutex::new(MediaEngine::default())),
            api: Arc::new(api),
            ice_transport: ice,
            peer_meta: meta,
            sctp_transport: Arc::clone(&sctp),
            dtls_transport: dtls,
            ice_role: Arc::new(RTCIceRole::default()),
            ready: false,
            rtp_senders: Vec::new(),
            rtp_receivers: Vec::new(),
            pending_requests: Arc::new(Mutex::new(BTreeMap::new())),
            local_tracks: Vec::new(),

            //signaling_dc: Arc::clone(&signaling_dc),
            ice_gatherer: gatherer,
            //dc_index: 0,

            on_close_handler: Arc::new(Mutex::new(None)),
            //on_data_channel_handler: Arc::new(Mutex::new(None)),
            //on_ready_handler: Arc::new(Mutex::new(None)),
            //on_track_handler: Arc::new(Mutex::new(None)),
        })
    }

    pub async fn on_close(&self, f: OnPeerCloseFn) {
        let mut handler = self.on_close_handler.lock().await;
        *handler = Some(f);
    }

    pub async fn answer(&mut self, request: String) -> Result<String> {
        if self.ice_gatherer.state() != RTCIceGathererState::New {
            return Err(Error::ErrRelayPeerSignalDone.into());
        }
        let (gather_finished_tx, mut gather_finished_rx) = tokio::sync::mpsc::channel::<()>(1);
        let mut gather_finished_tx = Some(gather_finished_tx);
        self.ice_gatherer
            .on_local_candidate(Box::new(move |c: Option<RTCIceCandidate>| {
                if c.is_none() {
                    gather_finished_tx.take();
                }
                Box::pin(async {})
            }));

        self.ice_gatherer.gather().await?;

        let _ = gather_finished_rx.recv().await;

        let ice_candidates = self.ice_gatherer.get_local_candidates().await?;
        let ice_parameters = self.ice_gatherer.get_local_parameters().await?;
        let dtls_parameters = self.dtls_transport.get_local_parameters()?;
        let sctp_capabilities = self.sctp_transport.get_capabilities();

        let signal = Signal {
            ice_candidates,
            ice_parameters,
            dtls_parameters,
            sctp_capabilities,
            encodings: None,
            track_meta: None,
        };

        self.ice_role = Arc::new(RTCIceRole::Controlled);

        let signal_2 = serde_json::from_str::<Signal>(&request)?;

        self.start(signal_2).await?;

        let json_str = serde_json::to_string(&signal)?;

        Ok(json_str)
    }

    async fn start(&mut self, sig: Signal) -> Result<()> {
        self.ice_transport
            .set_remote_candidates(&sig.ice_candidates)
            .await?;

        // Start the ICE transport
        self.ice_transport
            .start(&sig.ice_parameters, Some(*self.ice_role))
            .await?;

        // Start the DTLS transport
        self.dtls_transport
            .start(sig.dtls_parameters.clone())
            .await?;

        // Start the SCTP transport
        self.sctp_transport.start(sig.sctp_capabilities).await?;

        self.ready = true;
        Ok(())
    }

    pub async fn close(&mut self) -> Vec<Result<()>> {
        let mut results: Vec<Result<()>> = Vec::new();
        for sender in &self.rtp_senders {
            match sender.stop().await {
                Err(err) => {
                    results.push(Err(err.into()));
                }
                Ok(_) => results.push(Ok(())),
            }
        }

        for receiver in &self.rtp_receivers {
            match receiver.stop().await {
                Err(err) => {
                    results.push(Err(err.into()));
                }
                Ok(_) => results.push(Ok(())),
            }
        }

        match self.sctp_transport.stop().await {
            Err(err) => {
                results.push(Err(err.into()));
            }
            Ok(_) => results.push(Ok(())),
        }

        match self.dtls_transport.stop().await {
            Err(err) => {
                results.push(Err(err.into()));
            }
            Ok(_) => results.push(Ok(())),
        }

        match self.ice_transport.stop().await {
            Err(err) => {
                results.push(Err(err.into()));
            }
            Ok(_) => results.push(Ok(())),
        }

        let mut handler = self.on_close_handler.lock().await;
        if let Some(f) = &mut *handler {
            f().await;
        }

        results
    }

    #[allow(dead_code)]
    async fn receive(&mut self, s: Signal) -> Result<()> {
        let codec_type: RTPCodecType;

        let codec_parameters = s.track_meta.clone().unwrap().codec_parameters.unwrap();
        if codec_parameters.capability.mime_type.starts_with("audio/") {
            codec_type = RTPCodecType::Audio;
        } else if codec_parameters.capability.mime_type.starts_with("video/") {
            codec_type = RTPCodecType::Video;
        } else {
            codec_type = RTPCodecType::Unspecified;
        }

        self.media_engine
            .try_lock()
            .unwrap()
            .register_codec(codec_parameters.to_owned(), codec_type)?;

        let rtp_receiver = self.api.new_rtp_receiver(
            codec_type,
            Arc::clone(&self.dtls_transport),
            Arc::new(NoOp {}),
        );

        let mut encodings = vec![];
        let coding_parameters = s.encodings.unwrap();
        if coding_parameters.ssrc != 0 {
            encodings.push(RTCRtpCodingParameters {
                ssrc: coding_parameters.ssrc,
                ..Default::default()
            });
        }
        encodings.push(RTCRtpCodingParameters {
            rid: coding_parameters.rid,
            ..Default::default()
        });

        rtp_receiver
            .receive(&RTCRtpReceiveParameters { encodings })
            .await?;

        rtp_receiver
            .set_rtp_parameters(RTCRtpParameters {
                header_extensions: Vec::new(),
                codecs: vec![codec_parameters],
            })
            .await;

        let arc_rtp_receiver = Arc::new(rtp_receiver);

        /* 
        if let Some(track) = arc_rtp_receiver.track().await {
            let mut handler = self.on_track_handler.lock().await;
            if let Some(f) = &mut *handler {
                f(track, Arc::clone(&arc_rtp_receiver), s.track_meta.unwrap()).await;
            }
        }*/

        self.rtp_receivers.push(arc_rtp_receiver);

        Ok(())
    }

    pub async fn add_track(
        &mut self,
        receiver: Arc<RTCRtpReceiver>,
        remote_track: Arc<TrackRemote>,
        local_track: Arc<dyn TrackLocal + Send + Sync>,
    ) -> Result<Arc<RTCRtpSender>> {
        let codec = remote_track.codec();

        let sdr = self
            .api
            .new_rtp_sender(
                Some(Arc::clone(&local_track)),
                Arc::clone(&self.dtls_transport),
                Arc::new(NoOp {}),
            )
            .await;

        self.media_engine
            .lock()
            .await
            .register_codec(codec.clone(), remote_track.kind())?;

        let track_meta = TrackMeta {
            stream_id: remote_track.stream_id(),
            track_id: remote_track.id(),
            codec_parameters: Some(codec),
        };

        let encodings = RTCRtpCodingParameters {
            ssrc: sdr.get_parameters().await.encodings[0].ssrc,
            payload_type: remote_track.payload_type(),
            ..Default::default()
        };

        info!("Creating relay signal");

        let signal = Signal {
            encodings: Some(encodings.clone()),
            ice_candidates: Vec::new(),
            ice_parameters: RTCIceParameters::default(),
            dtls_parameters: DTLSParameters::default(),
            sctp_capabilities: SCTPTransportCapabilities {
                max_message_size: 0,
            },
            track_meta: Some(track_meta),
        };

        let payload = serde_json::to_string(&signal)?;

        let timeout_duration = Duration::from_secs(2);

        info!("Sending relay request to clients");
        self.request(
            timeout_duration,
            RoomEvent::RelayPeerRequest { payload, room_id: self.peer_meta.room_id.clone() }
        )
        .await?;

        let parameters = receiver.get_parameters().await.clone();

        let send_parameters = RTCRtpSendParameters {
            rtp_parameters: parameters.clone(),
            encodings: vec![RTCRtpCodingParameters {
                ssrc: encodings.ssrc,
                payload_type: encodings.payload_type,
                ..Default::default()
            }],
        };

        sdr.send(&send_parameters).await?;

        self.local_tracks.push(local_track);

        let sdr_arc = Arc::new(sdr);

        self.rtp_senders.push(sdr_arc.clone());

        info!("Track added to relay peer");
        Ok(sdr_arc)
    }

    pub async fn reply(&self, event: RoomEvent) -> Result<()> {
        
        //let req_json = serde_json::to_string(&req)?;
        //self.signaling_dc.send(&Bytes::from(req_json)).await?;
        let room = Room::get(&self.peer_meta.room_id);
        room.publish(event).await;
        Ok(())
    }

    async fn request(
        &self,
        timeout: Duration,
        event: RoomEvent,
    ) -> Result<Vec<u8>> {
        let id = Ulid::new().to_string();

        // Send to room
        let room = Room::get(&self.peer_meta.room_id);
        room.publish(event).await;

        let timer = tokio::time::sleep(timeout);
        tokio::pin!(timer);

        let (event_publisher, mut event_consumer) = mpsc::unbounded_channel();
        self.pending_requests
            .lock()
            .await
            .insert(id.clone(), event_publisher);

        tokio::select! {
            _ = timer.as_mut() =>{
                self.pending_requests.lock().await.remove(&id);
                Err(Error::ErrRelayRequestTimeout.into())
            },
            data = event_consumer.recv() => {
                self.pending_requests.lock().await.remove(&id);

                if let Some(payload)  = data{
                    Ok(payload)
                }else{
                    Err(Error::ErrRelayRequestEmptyRespose.into())
                }
            },
        }
    }
}