use std::{sync::Arc, time::Duration};

use tokio::{net::UdpSocket, sync::Mutex};
use webrtc::{api::setting_engine::SettingEngine, ice::{mdns::MulticastDnsMode, udp_mux::{UDPMuxDefault, UDPMuxParams}, udp_network::{EphemeralUDP, UDPNetwork}}, ice_transport::{ice_candidate_type::RTCIceCandidateType, ice_server::RTCIceServer}, peer_connection::{configuration::RTCConfiguration, policy::sdp_semantics::RTCSdpSemantics}, turn::auth::AuthHandler,};
use anyhow::Result;

use crate::turn::{self, TurnConfig};
use crate::{buffer::factory::AtomicFactory, track::error::ConfigError};

#[derive(Clone, Deserialize)]
struct ICEServerConfig {
    urls: Vec<String>,
    user_name: String,
    credential: String,
}
#[derive(Clone, Default, Deserialize)]
struct Candidates {
    #[serde(rename = "icelite")]
    ice_lite: Option<bool>,
    #[serde(rename = "nat1to1ips")]
    nat1_to_1ips: Option<Vec<String>>,
}
#[derive(Default)]
pub struct WebRTCTransportConfig {
    pub configuration: RTCConfiguration,
    pub setting: SettingEngine,
    pub router: RouterConfig,
    pub factory: Arc<Mutex<AtomicFactory>>,
}
#[derive(Clone, Default, Deserialize)]
struct WebRTCTimeoutsConfig {
    #[serde(rename = "disconnected")]
    ice_disconnected_timeout: i32,
    #[serde(rename = "failed")]
    ice_failed_timeout: i32,
    #[serde(rename = "keepalive")]
    ice_keepalive_interval: i32,
}
#[derive(Clone, Default, Deserialize)]
pub struct WebRTCConfig {
    ice_single_port: Option<i32>,
    #[serde(rename = "portrange")]
    pub ice_port_range: Option<Vec<u16>>,
    ice_servers: Option<Vec<ICEServerConfig>>,
    candidates: Candidates,
    #[serde(rename = "sdpsemantics")]
    pub sdp_semantics: String,
    #[serde(rename = "mdns")]
    mdns: bool,
    timeouts: WebRTCTimeoutsConfig,
}

#[derive(Default, Clone, Deserialize, Debug)]
pub struct SimulcastConfig {
    #[serde(rename = "bestqualityfirst")]
    pub best_quality_first: bool,
}

#[derive(Default, Clone, Deserialize, Debug)]
pub struct RouterConfig {
    #[serde(rename = "withstats")]
    pub with_stats: bool,
    #[serde(rename = "maxbandwidth")]
    pub max_bandwidth: u64,
    #[serde(rename = "maxpackettrack")]
    pub max_packet_track: i32,
    #[serde(rename = "audiolevelinterval")]
    pub audio_level_interval: i32,
    #[serde(rename = "audiolevelthreshold")]
    #[allow(dead_code)]
    audio_level_threshold: u8,
    #[serde(rename = "audiolevelfilter")]
    #[allow(dead_code)]
    audio_level_filter: i32,
    pub simulcast: SimulcastConfig,
}

#[derive(Clone, Default, Deserialize)]
pub struct Config {
    router: RouterConfig,
    pub webrtc: WebRTCConfig,
    pub turn: TurnConfig,
    #[serde(skip_deserializing)]
    pub turn_auth: Option<Arc<dyn AuthHandler + Send + Sync>>,
}


pub fn load(content: &str) -> Result<Config, ConfigError> {
    let decoded_config = toml::from_str(content).unwrap();
    Ok(decoded_config)
}

impl WebRTCTransportConfig {
    pub async fn new(c: &Config) -> Result<Self> {
        let mut se = SettingEngine::default();
        se.disable_media_engine_copy(true);

        if let Some(ice_single_port) = c.webrtc.ice_single_port {
            info!("Binding UDP socket to 0.0.0.0:{ice_single_port}");
            let rv = UdpSocket::bind(("0.0.0.0", ice_single_port as u16)).await;
            let udp_socket: UdpSocket = match rv {
                Ok(sock) => sock,
                Err(_) => {
                    std::process::exit(0);
                }
            };
            let udp_mux = UDPMuxDefault::new(UDPMuxParams::new(udp_socket));
            se.set_udp_network(UDPNetwork::Muxed(udp_mux));
        } else {
            let mut ice_port_start: u16 = 0;
            let mut ice_port_end: u16 = 0;

            if c.turn.enabled && c.turn.port_range.is_none() {
                ice_port_start = turn::ICE_MIN_PORT;
                ice_port_end = turn::ICE_MAX_PORT;
            } else if let Some(ice_port_range) = &c.webrtc.ice_port_range {
                if ice_port_range.len() == 2 {
                    ice_port_start = ice_port_range[0];
                    ice_port_end = ice_port_range[1];
                }
            }

            if ice_port_start != 0 || ice_port_end != 0 {
                info!("Creating UDP network on port range from {ice_port_start} to {ice_port_end}");
                let ephemeral_udp =
                    UDPNetwork::Ephemeral(EphemeralUDP::new(ice_port_start, ice_port_end).unwrap());
                se.set_udp_network(ephemeral_udp);
            }
        }

        
        let mut ice_servers: Vec<RTCIceServer> = vec![RTCIceServer {
            urls: vec!["stun:stun.l.google.com:19302".to_owned(), "stun:stun1.l.google.com:19302".to_owned(), "stun:stun.12connect.com:3478".to_owned()],
            ..Default::default()
        }];

        if let Some(ice_lite) = c.webrtc.candidates.ice_lite {
            if ice_lite {
                se.set_lite(ice_lite);
            } else if let Some(ice_servers_cfg) = &c.webrtc.ice_servers {
                for ice_server in ice_servers_cfg {
                    let s = RTCIceServer {
                        urls: ice_server.urls.clone(),
                        username: ice_server.user_name.clone(),
                        credential: ice_server.credential.clone(),
                    };

                    ice_servers.push(s);
                }
            }
        }

        let mut _sdp_semantics = RTCSdpSemantics::UnifiedPlan;

        match c.webrtc.sdp_semantics.as_str() {
            "unified-plan-with-fallback" => {
                _sdp_semantics = RTCSdpSemantics::UnifiedPlanWithFallback;
            }
            "plan-b" => {
                _sdp_semantics = RTCSdpSemantics::PlanB;
            }
            _ => {}
        }

        if c.webrtc.timeouts.ice_disconnected_timeout == 0
            && c.webrtc.timeouts.ice_failed_timeout == 0
            && c.webrtc.timeouts.ice_keepalive_interval == 0
        {
        } else {
            se.set_ice_timeouts(
                Some(Duration::from_secs(
                    c.webrtc.timeouts.ice_disconnected_timeout as u64,
                )),
                Some(Duration::from_secs(
                    c.webrtc.timeouts.ice_failed_timeout as u64,
                )),
                Some(Duration::from_secs(
                    c.webrtc.timeouts.ice_keepalive_interval as u64,
                )),
            );
        }

        let mut w = WebRTCTransportConfig {
            configuration: RTCConfiguration {
                ice_servers,
                ..Default::default()
            },
            setting: se,
            router: c.router.clone(),
            factory: Arc::new(Mutex::new(AtomicFactory::new(1000, 1000))),
        };

        if let Some(nat1toiips) = &c.webrtc.candidates.nat1_to_1ips {
            if !nat1toiips.is_empty() {
                w.setting
                    .set_nat_1to1_ips(nat1toiips.clone(), RTCIceCandidateType::Host);
            }
        }

        if c.webrtc.mdns {
            w.setting
                .set_ice_multicast_dns_mode(MulticastDnsMode::Disabled);
        }

        info!("WebRTCTransport configuration finished");
        Ok(w)
    }
}