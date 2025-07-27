use std::{sync::Arc, time::Duration};

use tokio::{net::UdpSocket, sync::Mutex};
use webrtc::{api::setting_engine::SettingEngine, ice::{mdns::MulticastDnsMode, udp_mux::{UDPMuxDefault, UDPMuxParams}, udp_network::{UDPNetwork}}, ice_transport::{ice_candidate_type::RTCIceCandidateType, ice_server::RTCIceServer}, peer_connection::{configuration::RTCConfiguration, policy::sdp_semantics::RTCSdpSemantics}};
use anyhow::Result;

use crate::{buffer::AtomicFactory};

#[cfg(feature = "turn")]
use crate::turn::TurnConfig;
#[cfg(feature = "turn")]
use webrtc::turn::auth::AuthHandler;

// 4096 port range
pub const ICE_MIN_PORT: u16 = 36864;
pub const ICE_MAX_PORT: u16 = 40959;

#[derive(Clone, Deserialize)]
struct ICEServerConfig {
    urls: Vec<String>,
    #[serde(default)]
    username: String,
    #[serde(default)]
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
    pub version: String,
    pub configuration: RTCConfiguration,
    pub setting: SettingEngine,
    pub router: RouterConfig,
    pub factory: Arc<Mutex<AtomicFactory>>,
    pub mux_port: Option<u16>,
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
    ice_single_port: Option<u16>,
    #[serde(rename = "portrange")]
    pub ice_port_range: Option<Vec<u16>>,
    #[serde(rename = "iceservers")]
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
    pub max_packet_track: u32,
    #[serde(rename = "audiolevelinterval")]
    pub audio_level_interval: i32,
    #[serde(rename = "audiolevelthreshold")]
    pub audio_level_threshold: u8,
    #[serde(rename = "audiolevelfilter")]
    pub audio_level_filter: i32,
    pub simulcast: SimulcastConfig,
}

#[cfg(not(feature = "turn"))]
#[derive(Deserialize, Clone, Default)]
pub struct TurnConfig {
    pub enabled: bool,
}

#[derive(Clone, Default, Deserialize)]
pub struct Config {
    router: RouterConfig,
    pub webrtc: WebRTCConfig,
    pub turn: TurnConfig,
    #[cfg(feature = "turn")]
    #[serde(skip_deserializing)]
    pub turn_auth: Option<Arc<dyn AuthHandler + Send + Sync>>,
}

impl Config {

    /// Parses provided file content as TOML.
    #[cfg(feature="toml")]
    pub fn from_toml(content: &str) -> Result<Config, toml::de::Error> {
        toml::from_str(content)
    }
}

impl WebRTCTransportConfig {

    /// Creates the initial configuration for running RTC peer connections
    /// # Recommendations
    /// - Use `webrtc.ice_single_port` for internal RTCP packets handling. `webrtc.ice_port_range` is harder to handle.
    /// - Use [Self::bind] to start the UDP socket. 
    pub fn new(c: &Config) -> Self {
        let mut se = SettingEngine::default();
        se.disable_media_engine_copy(true);

        let mux_port = c.webrtc.ice_single_port;

        if c.webrtc.ice_port_range.is_some() {
            warn!("Epehemeral network is deprecated and will be not be handled by this crate.");
        }

        if c.turn.enabled {
            error!("`turn` feature is not enabled for this crate. Turn server will not be started.");
        }
        
        let mut ice_servers: Vec<RTCIceServer> = Vec::default();
        let ice_lite = c.webrtc.candidates.ice_lite.unwrap_or_default();
        se.set_lite(ice_lite);

        if !ice_lite {
            if let Some(ice_servers_cfg) = &c.webrtc.ice_servers {
                    for ice_server in ice_servers_cfg {
                        let s = RTCIceServer {
                            urls: ice_server.urls.clone(),
                            username: ice_server.username.clone(),
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

        if let Some(nat1toiips) = &c.webrtc.candidates.nat1_to_1ips {
            if !nat1toiips.is_empty() {
                se.set_nat_1to1_ips(nat1toiips.clone(), RTCIceCandidateType::Host);
            }
        }
        
        if c.webrtc.mdns {
            se.set_ice_multicast_dns_mode(MulticastDnsMode::Disabled);
        }

        WebRTCTransportConfig {
            configuration: RTCConfiguration {
                ice_servers,
                ..Default::default()
            },
            setting: se,
            router: c.router.clone(),
            factory: Arc::default(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            mux_port,
        }
    }

    /// Binds the UDP network to receive data.
    pub async fn bind(&mut self) -> Result<()> {

        let se = &mut self.setting;

        if let Some(ice_single_port) = self.mux_port {
            info!("Binding UDP socket to 0.0.0.0:{ice_single_port}");
            let udp_socket = UdpSocket::bind(("0.0.0.0", ice_single_port)).await?;
            let udp_mux = UDPMuxDefault::new(UDPMuxParams::new(udp_socket));
            se.set_udp_network(UDPNetwork::Muxed(udp_mux));
        }

        Ok(())
    }
}