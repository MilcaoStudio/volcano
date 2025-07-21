use std::sync::{atomic::{AtomicBool, Ordering}, Arc};

use webrtc::{
    api::{
        interceptor_registry::register_default_interceptors,
        media_engine::MediaEngine,
        APIBuilder,
    }, data_channel::{data_channel_init::RTCDataChannelInit, RTCDataChannel}, error::Result, interceptor::registry::Registry, peer_connection::{configuration::RTCConfiguration, RTCPeerConnection}, rtp_transceiver::rtp_codec::{RTCRtpHeaderExtensionCapability, RTPCodecType}, sdp::extmap, track::track_local::TrackLocal
};

use crate::{rtc::{config::WebRTCTransportConfig, message::RemoteMedia}, track::downtrack::DownTrack};
use super::API_CHANNEL_LABEL;

const FRAME_MARKING: &str = "urn:ietf:params:rtp-hdrext:framemarking";

const HIGH_VALUE: &str = "high";
const MEDIA_VALUE: &str = "medium";
const LOW_VALUE: &str = "low";
const MUTED_VALUE: &str = "none";

/// Creates a new basic [RTCPeerConnection] using default codecs and interceptors.
/// # Configuration
/// Subscriber connection is configured with:
/// - ICE Servers from [WebRTCTransportConfig::configuration]
pub async fn create_subscriber_connection(cfg: &Arc<WebRTCTransportConfig>) -> Result<Arc<RTCPeerConnection>> {
    // Create a MediaEngine object to configure the supported codec
    let mut m = MediaEngine::default();
    m.register_default_codecs()?;
   
    // Create a InterceptorRegistry. This is the user configurable RTP/RTCP Pipeline.
    // This provides NACKs, RTCP Reports and other features. If you use `webrtc.NewPeerConnection`
    // this is enabled by default. If you are manually managing You MUST create a InterceptorRegistry
    // for each PeerConnection.
    let registry = register_default_interceptors(Registry::new(), &mut m)?;

    // Create the API object with the MediaEngine
    let api = APIBuilder::new()
        .with_media_engine(m)
        .with_interceptor_registry(registry)
        //.with_setting_engine()
        .build();

    // Create a new RTCPeerConnection
    api.new_peer_connection(RTCConfiguration {
        ice_servers: cfg.configuration.ice_servers.clone(),
        ..Default::default()
    })
        .await
        .map(Arc::new)
}

/// Creates a new [RTCPeerConnection] using the default codecs, default interceptos and the provided [WebRTCTransportConfig].
/// # Configuration
/// Publisher connection is configured with:
/// - [WebRTCTransportConfig::setting] for the media engine.
/// - [WebRTCTransportConfig::configuration] for the peer connection.
pub async fn create_publisher_connection(cfg: WebRTCTransportConfig) -> Result<Arc<RTCPeerConnection>> {
    let mut m = MediaEngine::default();
    m.register_default_codecs()?;
   
    set_header_extensions(&mut m);

    let setting_engine = cfg.setting.clone();
    let api = APIBuilder::new()
            .with_media_engine(m)
            .with_setting_engine(setting_engine)
            .build();
    api.new_peer_connection(cfg.configuration)
        .await
        .map(Arc::new)
}

/// Creates a new [RTCPeerConnection] using the default codecs, default interceptos and the provided [WebRTCTransportConfig].
/// # Configuration
/// Central connection is configured with:
/// - [WebRTCTransportConfig::setting] for the media engine.
/// - [WebRTCTransportConfig::configuration] for the peer connection.
pub async fn create_central_connection(cfg: Arc<WebRTCTransportConfig>) -> Result<Arc<RTCPeerConnection>> {
    let mut m = MediaEngine::default();
    m.register_default_codecs()?;
    set_header_extensions(&mut m);
    let registry = register_default_interceptors(Registry::new(), &mut m)?;
    let setting_engine = cfg.setting.clone();
    let api = APIBuilder::new()
        .with_media_engine(m)
        .with_interceptor_registry(registry)
        .with_setting_engine(setting_engine)
        .build();
    api.new_peer_connection(cfg.configuration.clone())
        .await
        .map(Arc::new)
}

fn set_header_extensions(m: &mut MediaEngine) {
    let extensions_video = vec![
        extmap::SDES_MID_URI,
        extmap::SDES_RTP_STREAM_ID_URI,
        extmap::TRANSPORT_CC_URI,
        extmap::VIDEO_ORIENTATION_URI,
        FRAME_MARKING,
    ];

    for ext in extensions_video {
        if let Err(err) = m.register_header_extension(
            RTCRtpHeaderExtensionCapability {
                uri: String::from(ext),
            },
            RTPCodecType::Video,
            None,
        ) {
            error!("Register ext {ext} failed: {err}");
        };
    }

    let extensions_audio = vec![
        extmap::SDES_MID_URI,
        extmap::SDES_RTP_STREAM_ID_URI,
        extmap::AUDIO_LEVEL_URI,
    ];

    for ext in extensions_audio {
        if let Err(err) = m.register_header_extension(
            RTCRtpHeaderExtensionCapability {
                uri: String::from(ext),
            },
            RTPCodecType::Audio,
            None,
        ) {
            error!("Register ext {ext} failed: {err}");
        };
    }
}

pub async fn create_api_data_channel(pc: &RTCPeerConnection, id: String, open: Arc<AtomicBool>) -> Result<Arc<RTCDataChannel>> {
    let api = pc.create_data_channel(API_CHANNEL_LABEL, Some(RTCDataChannelInit::default())).await;
    info!("Created data channel `{API_CHANNEL_LABEL}` (awaiting for offer)");
    match api {
        Ok(channel) => {
            let open_1 = open.clone();
            let id_out_1 = id.clone();
            let id_out_2 = id.clone();
            channel.on_open(Box::new(move || {
                Box::pin(async move {
                    info!("[Peer {id_out_1}] API data channel open");
                    open_1.store(true, Ordering::Release);
                })
            }));

            let open_2 = open.clone();
            channel.on_close(Box::new(move || {
                let open_in = open_2.clone();
                let id_in = id_out_2.clone();
                Box::pin(async move {
                    info!("[Peer {id_in}] API data channel closed");
                    open_in.store(false, Ordering::Release);
                })
            }));

            Ok(channel)
        },
        Err(err) => Err(err.into()),
    }
}


pub async fn process_remote_media(remote_media: &RemoteMedia, down_tracks: &Vec<Arc<DownTrack>>) {
    if let Some(layers) = &remote_media.layers {
        if !layers.is_empty() {
            return;
        }
    }
    for dt in down_tracks {
        match dt.kind() {
            RTPCodecType::Audio => dt.mute(!remote_media.audio),
            RTPCodecType::Video => {
                match remote_media.video.as_str() {
                    HIGH_VALUE => {
                        dt.mute(false);
                        if let Err(err) = dt.switch_spatial_layer(2, true).await {
                            error!("switch_spatial_layer err: {}", err);
                        }
                    }
                    MEDIA_VALUE => {
                        dt.mute(false);
                        if let Err(err) = dt.switch_spatial_layer(1, true).await {
                            error!("switch_spatial_layer err: {}", err);
                        }
                    }
                    LOW_VALUE => {
                        dt.mute(false);
                        if let Err(err) = dt.switch_spatial_layer(0, true).await {
                            error!("switch_spatial_layer err: {}", err);
                        }
                    }
                    MUTED_VALUE => {
                        dt.mute(true);
                    }
                    _ => {
                        warn!("remote_media.video \"{}\" unrecognized", remote_media.video);
                    }
                }

                match remote_media.frame_rate.as_str() {
                    HIGH_VALUE => dt.switch_temporal_layer(3, true).await,
                    MEDIA_VALUE => dt.switch_temporal_layer(2, true).await,
                    LOW_VALUE => dt.switch_temporal_layer(1, true).await,
                    _ => {}
                }
            }
            RTPCodecType::Unspecified => {}
        }
    }
}