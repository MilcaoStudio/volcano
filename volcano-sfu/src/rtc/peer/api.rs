use std::sync::Arc;

use webrtc::{
    api::{
        interceptor_registry::register_default_interceptors, media_engine::MediaEngine, APIBuilder,
    }, error::Result, interceptor::registry::Registry, peer_connection::{configuration::RTCConfiguration, RTCPeerConnection}, rtp_transceiver::{
        rtp_codec::{RTCRtpCodecCapability, RTCRtpCodecParameters, RTCRtpHeaderExtensionCapability, RTPCodecType},
        RTCPFeedback,
    }, sdp::extmap
};

use crate::rtc::config::WebRTCTransportConfig;

const MIME_TYPE_H264: &str = "video/h264";
const MIME_TYPE_OPUS: &str = "audio/opus";
const MIME_TYPE_VP8: &str = "video/vp8";
const MIME_TYPE_VP9: &str = "video/vp9";

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
    let mut registry = Registry::new();

    // Use the default set of Interceptors
    registry = register_default_interceptors(registry, &mut m)?;

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
        .map_err(Into::into)
}

pub async fn create_publisher_connection(cfg: WebRTCTransportConfig) -> Result<Arc<RTCPeerConnection>> {
    let mut m = MediaEngine::default();
    m.register_codec(RTCRtpCodecParameters {
        capability: RTCRtpCodecCapability {
            mime_type: String::from(MIME_TYPE_OPUS),
            clock_rate: 48000,
            channels: 2,
            sdp_fmtp_line: String::from("minptime=10;useinbandfec=1"),
            rtcp_feedback: Vec::new(),
        },
        payload_type: 111,
        ..Default::default()
    }, RTPCodecType::Audio)?;
    let feedbacks = vec![
        RTCPFeedback {
            typ: String::from("goog-remb"),
            parameter: String::from(""),
        },
        RTCPFeedback {
            typ: String::from("ccm"),
            parameter: String::from("fir"),
        },
        RTCPFeedback {
            typ: String::from("nack"),
            parameter: String::from(""),
        },
        RTCPFeedback {
            typ: String::from("nack"),
            parameter: String::from("pli"),
        },
    ];

    let codc_parameters = vec![
        RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: String::from(MIME_TYPE_VP8),
                clock_rate: 90000,
                rtcp_feedback: feedbacks.clone(),
                ..Default::default()
            },
            payload_type: 96,
            ..Default::default()
        },
        RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: String::from(MIME_TYPE_VP9),
                clock_rate: 90000,
                sdp_fmtp_line: String::from("profile-id=0"),
                rtcp_feedback: feedbacks.clone(),
                ..Default::default()
            },
            payload_type: 98,
            ..Default::default()
        },
        RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: String::from(MIME_TYPE_VP9),
                clock_rate: 90000,
                sdp_fmtp_line: String::from("profile-id=1"),
                rtcp_feedback: feedbacks.clone(),
                ..Default::default()
            },
            payload_type: 100,
            ..Default::default()
        },
        RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: String::from(MIME_TYPE_H264),
                clock_rate: 90000,
                sdp_fmtp_line: String::from(
                    "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42001f",
                ),
                rtcp_feedback: feedbacks.clone(),
                ..Default::default()
            },
            payload_type: 102,
            ..Default::default()
        },
        RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: String::from(MIME_TYPE_H264),
                clock_rate: 90000,
                sdp_fmtp_line: String::from(
                    "level-asymmetry-allowed=1;packetization-mode=0;profile-level-id=42001f",
                ),
                rtcp_feedback: feedbacks.clone(),
                ..Default::default()
            },
            payload_type: 127,
            ..Default::default()
        },
        RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: String::from(MIME_TYPE_H264),
                clock_rate: 90000,
                sdp_fmtp_line: String::from(
                    "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f",
                ),
                rtcp_feedback: feedbacks.clone(),
                ..Default::default()
            },
            payload_type: 125,
            ..Default::default()
        },
        RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: String::from(MIME_TYPE_H264),
                clock_rate: 90000,
                sdp_fmtp_line: String::from(
                    "level-asymmetry-allowed=1;packetization-mode=0;profile-level-id=42e01f",
                ),
                rtcp_feedback: feedbacks.clone(),
                ..Default::default()
            },
            payload_type: 108,
            ..Default::default()
        },
        RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: String::from(MIME_TYPE_VP8),
                clock_rate: 90000,
                sdp_fmtp_line: String::from(
                    "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=640032",
                ),
                rtcp_feedback: feedbacks,
                ..Default::default()
            },
            payload_type: 123,
            ..Default::default()
        },
    ];

    for codec in codc_parameters {
        m.register_codec(codec, RTPCodecType::Video)?;
    }
    let extensions_video = vec![
        extmap::SDES_MID_URI,
        extmap::SDES_RTP_STREAM_ID_URI,
        extmap::TRANSPORT_CC_URI,
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
    let setting_engine = cfg.setting.clone();
    let api = APIBuilder::new()
            .with_media_engine(m)
            .with_setting_engine(setting_engine)
            .build();
    api.new_peer_connection(cfg.configuration)
        .await
        .map(Arc::new)
        .map_err(Into::into)
}