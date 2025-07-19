use std::sync::Arc;

use webrtc::{
    api::{
        interceptor_registry::register_default_interceptors,
        media_engine::MediaEngine,
        APIBuilder,
    },
    error::Result,
    interceptor::registry::Registry,
    peer_connection::{configuration::RTCConfiguration, RTCPeerConnection},
    rtp_transceiver::rtp_codec::{RTCRtpHeaderExtensionCapability, RTPCodecType},
    sdp::extmap
};

use crate::rtc::config::WebRTCTransportConfig;

const FRAME_MARKING: &str = "urn:ietf:params:rtp-hdrext:framemarking";

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
pub async fn create_central_connection(cfg: WebRTCTransportConfig) -> Result<Arc<RTCPeerConnection>> {
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
    api.new_peer_connection(cfg.configuration)
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