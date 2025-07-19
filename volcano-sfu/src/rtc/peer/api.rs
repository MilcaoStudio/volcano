use std::sync::Arc;

use webrtc::{
    api::{
        interceptor_registry::register_default_interceptors,
        media_engine::{MediaEngine, MIME_TYPE_OPUS, MIME_TYPE_VP8},
        APIBuilder,
    },
    error::Result,
    interceptor::registry::Registry,
    peer_connection::{configuration::RTCConfiguration, RTCPeerConnection},
    rtp_transceiver::rtp_codec::{RTCRtpCodecCapability, RTCRtpCodecParameters, RTCRtpHeaderExtensionCapability, RTPCodecType},
    sdp::extmap
};

use crate::rtc::config::WebRTCTransportConfig;

const FRAME_MARKING: &str = "urn:ietf:params:rtp-hdrext:framemarking";

/// Initialise a new RTCPeerConnection
pub async fn create_subscriber_connection(cfg: &Arc<WebRTCTransportConfig>) -> Result<Arc<RTCPeerConnection>> {
    // Create a MediaEngine object to configure the supported codec
    let mut m = MediaEngine::default();
    //m.register_default_codecs()?;
    m.register_codec(
        RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: MIME_TYPE_VP8.to_owned(),
                clock_rate: 90000,
                channels: 0,
                sdp_fmtp_line: "".to_owned(),
                rtcp_feedback: vec![],
            },
            payload_type: 96,
            ..Default::default()
        },
        RTPCodecType::Video,
    )?;

    m.register_codec(
        RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: MIME_TYPE_OPUS.to_owned(),
                clock_rate: 48000,
                channels: 2,
                sdp_fmtp_line: "".to_owned(),
                rtcp_feedback: vec![],
            },
            payload_type: 111,
            ..Default::default()
        },
        RTPCodecType::Audio,
    )?;
   
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

pub async fn create_publisher_connection(cfg: WebRTCTransportConfig) -> Result<Arc<RTCPeerConnection>> {
    let mut m = MediaEngine::default();
    m.register_default_codecs()?;
   
    set_header_extensions(&mut m)?;

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
pub async fn create_central_connection(cfg: WebRTCTransportConfig) -> Result<Arc<RTCPeerConnection>> {
    let mut m = MediaEngine::default();
    m.register_default_codecs()?;
    set_header_extensions(&mut m)?;
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

fn set_header_extensions(m: &mut MediaEngine) -> Result<()> {
    let extensions_video = vec![
        extmap::SDES_MID_URI,
        extmap::SDES_RTP_STREAM_ID_URI,
        extmap::TRANSPORT_CC_URI,
        extmap::VIDEO_ORIENTATION_URI,
        FRAME_MARKING,
    ];

    for extention in extensions_video {
        m.register_header_extension(
            RTCRtpHeaderExtensionCapability {
                uri: String::from(extention),
            },
            RTPCodecType::Video,
            None,
        )?;
    }

    let extensions_audio = vec![
        extmap::SDES_MID_URI,
        extmap::SDES_RTP_STREAM_ID_URI,
        extmap::AUDIO_LEVEL_URI,
    ];

    for extention in extensions_audio {
        m.register_header_extension(
            RTCRtpHeaderExtensionCapability {
                uri: String::from(extention),
            },
            RTPCodecType::Audio,
            None,
        )?;
    }

    Ok(())
}