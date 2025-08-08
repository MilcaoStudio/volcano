use std::{sync::Arc, time::Duration};

use anyhow::Result;
use tokio::{net::UdpSocket, sync::Mutex};
use webrtc::{
    api::setting_engine::SettingEngine,
    ice::{
        mdns::MulticastDnsMode,
        network_type::{supported_network_types, NetworkType},
        udp_mux::{UDPMuxDefault, UDPMuxParams},
        udp_network::{EphemeralUDP, UDPNetwork}
    },
    ice_transport::{ice_candidate_type::RTCIceCandidateType, ice_server::RTCIceServer},
    peer_connection::{
        configuration::RTCConfiguration,
        policy::sdp_semantics::RTCSdpSemantics,
    },
};

use crate::buffer::AtomicFactory;

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
    #[serde(rename = "nat1to1")]
    nat1_to_1ips: Option<Vec<String>>,
    #[serde(rename = "disableipv4", default)]
    disable_ipv4: bool,
    #[serde(rename = "disableipv6", default)]
    disable_ipv6: bool,
}

#[derive(Copy, Clone)]
pub enum PortMap {
    /// Single port used for UDP Mux network
    Single(u16),
    // Port range used for UDP Ephemeral network
    Range(u16, u16),
}

impl Default for PortMap {
    fn default() -> Self {
        Self::Single(0)
    }
}

#[derive(Clone, Default)]
pub struct WebRTCTransportConfig {
    pub version: String,
    pub configuration: RTCConfiguration,
    pub ice_servers: Vec<RTCIceServer>,
    pub setting: SettingEngine,
    pub router: RouterConfig,
    pub factory: Arc<Mutex<AtomicFactory>>,
    pub port_map: PortMap,
}

#[derive(Clone, Default, Deserialize)]
struct WebRTCTimeoutsConfig {
    #[serde(rename = "disconnected")]
    ice_disconnected_timeout: u64,
    #[serde(rename = "failed")]
    ice_failed_timeout: u64,
    #[serde(rename = "keepalive")]
    ice_keepalive_interval: u64,
}
#[derive(Clone, Default, Deserialize)]
pub struct WebRTCConfig {
    #[serde(rename = "singleport")]
    single_port: Option<u16>,
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
    #[cfg(feature = "toml")]
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

        let port_map = if let Some(single_port) = c.webrtc.single_port {
            PortMap::Single(single_port)
        } else if let Some(ports) = &c.webrtc.ice_port_range {
            assert!(
                ports.len() > 1,
                "Expected at least 2 elements in webrtc.ice_port_range"
            );
            PortMap::Range(ports[0], ports[1])
        } else {
            panic!("Expected either webrtc.ice_single_port or webrtc.ice_port_range");
        };

        if c.turn.enabled {
            error!(
                "`turn` feature is not enabled for this crate. Turn server will not be started."
            );
        }

        let ice_lite = c.webrtc.candidates.ice_lite.unwrap_or_default();
        debug!("SFU config: ICE lite {ice_lite}");
        se.set_lite(ice_lite);
        
        let mut ice_servers: Vec<RTCIceServer> = Vec::default();
        // Clone and move
        if let Some(ice_servers_cfg) = c.webrtc.ice_servers.clone() {
            ice_servers = ice_servers_cfg.into_iter()
                .map(|s| RTCIceServer {
                    urls: s.urls,
                    username: s.username,
                    credential: s.credential,
                }).collect();
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

        let disconnected_timeout = if c.webrtc.timeouts.ice_disconnected_timeout > 0 {
            Some(Duration::from_secs(c.webrtc.timeouts.ice_disconnected_timeout))
        } else {
            None
        };
        
        let failed_timeout = if c.webrtc.timeouts.ice_failed_timeout > 0 {
            Some(Duration::from_secs(c.webrtc.timeouts.ice_failed_timeout))
        } else {
            None
        };
        
        let keep_alive_interval = if c.webrtc.timeouts.ice_keepalive_interval > 0 {
            Some(Duration::from_secs(c.webrtc.timeouts.ice_keepalive_interval))
        } else {
            None
        };
        
        se.set_ice_timeouts(disconnected_timeout, failed_timeout, keep_alive_interval);

        if let Some(nat1toiips) = c.webrtc.candidates.nat1_to_1ips.clone() {
            let has_ips = nat1toiips.is_empty();
            if !has_ips {
                debug!("SFU config: Set {:?} for NAT 1-to-1 [Host]", nat1toiips);
                se.set_nat_1to1_ips(nat1toiips, RTCIceCandidateType::Host);
            }

            // Disable mDNS when using 1to1
            let mdns = if c.webrtc.mdns && !has_ips {
                MulticastDnsMode::QueryAndGather
            } else {
                MulticastDnsMode::Disabled
            };
            se.set_ice_multicast_dns_mode(mdns);
        }

        let candidates = &c.webrtc.candidates;
        let network_types = if candidates.disable_ipv6 {
            debug!("SFU config: IPv6 disabled");
            vec![NetworkType::Udp4]
        } else if candidates.disable_ipv4 {
            debug!("SFU config: IPv4 disabled");
            vec![NetworkType::Udp6]
        } else {
            // Same effect as returning an empty vector
            supported_network_types()
        };

        se.set_network_types(network_types);

        WebRTCTransportConfig {
            configuration: RTCConfiguration {
                ice_servers: if !ice_lite {
                    ice_servers.clone()
                } else {
                    Vec::default()
                },
                ..Default::default()
            },
            ice_servers,
            setting: se,
            router: c.router.clone(),
            factory: Arc::default(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            port_map,
        }
    }

    /// Binds the UDP network to receive data.
    pub async fn bind_udp(&mut self) -> Result<()> {
        let se = &mut self.setting;

        let network = match self.port_map {
            PortMap::Single(port) => {
                let udp_socket = UdpSocket::bind(("0.0.0.0", port)).await?;
                // If port 0, set bound port
                if let Ok((address, bind_port)) =
                    udp_socket.local_addr().map(|ip| (ip.ip(), ip.port()))
                {
                    debug!("UDP socket (mux) bound to {address}:{bind_port}");
                    self.port_map = PortMap::Single(bind_port);
                };
                let udp_mux = UDPMuxDefault::new(UDPMuxParams::new(udp_socket));
                UDPNetwork::Muxed(udp_mux)
            }
            PortMap::Range(min, max) => {
                let ephemeral = EphemeralUDP::new(min, max)?;
                UDPNetwork::Ephemeral(ephemeral)
            }
        };
        se.set_udp_network(network);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{net::{Ipv4Addr, SocketAddrV4}};

    use tokio::net::UdpSocket;


    #[tokio::test]
    async fn test_udp_loopback() -> Result<(), Box<dyn std::error::Error>> {
        let ip: Ipv4Addr = "127.0.0.1".parse()?;
        let listen_addr = SocketAddrV4::new(ip, 60000);
        let send_addr = SocketAddrV4::new(ip, 60001);
        let listener = UdpSocket::bind(listen_addr).await?;
    
        let sender = UdpSocket::bind(send_addr).await?;
        sender.send_to(b"STUN-TEST", &listen_addr).await?;
    
        let mut buf = [0; 100];
        let (size, _) = listener.recv_from(&mut buf).await?;
        let msg = String::from_utf8_lossy(&buf[..size]);
    
        assert_eq!(msg, "STUN-TEST", "Loopback should work");
    
        Ok(())
    }
}