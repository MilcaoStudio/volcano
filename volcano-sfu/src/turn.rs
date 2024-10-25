use std::{collections::HashMap, net::{IpAddr, SocketAddr}, str::FromStr, sync::Arc, time::Duration};

use tokio::net::UdpSocket;
use webrtc::turn::{auth, auth::{AuthHandler, LongTermAuthHandler}, relay::relay_range::RelayAddressGeneratorRanges, server::{config::{ConnConfig, ServerConfig}, Server as TurnServer}, Error};
use webrtc::util::vnet::net::*;

// 4096 port range
pub const TURN_MIN_PORT: u16 = 32768;
pub const TURN_MAX_PORT: u16 = 36863;

// 4096 port range
pub const ICE_MIN_PORT: u16 = 36864;
pub const ICE_MAX_PORT: u16 = 40959;

#[derive(Clone, Default, Deserialize)]
pub(super) struct TurnAuth {
    #[serde(rename = "credentials")]
    #[allow(dead_code)]
    credentials: String,
    secret: Option<String>,
}
#[derive(Clone, Default, Deserialize)]
pub struct TurnConfig {
    #[serde(rename = "enabled")]
    pub enabled: bool,
    #[serde(rename = "realm")]
    realm: String,
    #[serde(rename = "address")]
    address: String,
    #[allow(dead_code)]
    cert: Option<String>,
    #[allow(dead_code)]
    key: Option<String>,
    auth: TurnAuth,
    pub(crate) port_range: Option<Vec<u16>>,
}

struct CustomAuthHandler {
    users_map: Arc<HashMap<String, Vec<u8>>>,
}

impl CustomAuthHandler {
    fn new(user_map: HashMap<String, Vec<u8>>) -> Self {
        Self {
            users_map: Arc::new(user_map),
        }
    }
}

impl AuthHandler for CustomAuthHandler {
    fn auth_handle(
        &self,
        username: &str,
        realm: &str,
        _src_addr: SocketAddr,
    ) -> Result<Vec<u8>, Error> {
        if let Some(val) = self.users_map.get(&username.to_string()) {
            return Ok(val.clone());
        }
        trace!("realm val: {}", realm);

        Err(Error::ErrNilConn)
    }
}

pub async fn init_turn_server(
    conf: TurnConfig,
    auth: Option<Arc<dyn AuthHandler + Send + Sync>>,
) -> anyhow::Result<TurnServer> {
    let conn = Arc::new(UdpSocket::bind(conf.address.clone()).await?);
    info!("UDP listening {}...", conn.local_addr()?);

    let mut new_auth: Option<Arc<dyn AuthHandler + Send + Sync>> = auth;

    if new_auth.is_none() {
        if let Some(secret) = conf.auth.secret {
            new_auth = Some(Arc::new(LongTermAuthHandler::new(secret)));
        } else {
            let mut users_map: HashMap<String, Vec<u8>> = HashMap::new();
            let re = regex::Regex::new(r"(\w+)=(\w+)").unwrap();

            for caps in re.captures_iter(conf.realm.clone().as_str()) {
                let username = caps.get(1).unwrap().as_str();
                let username_string = username.to_string();
                let password = caps.get(2).unwrap().as_str();
                users_map.insert(
                    username_string,
                    auth::generate_auth_key(username, conf.realm.clone().as_str(), password),
                );
            }

            if users_map.is_empty() {
                error!("no turn auth provided.");
            }

            new_auth = Some(Arc::new(CustomAuthHandler::new(users_map)))
        }
    }

    let mut min_port: u16 = TURN_MIN_PORT;
    let mut max_port: u16 = TURN_MAX_PORT;

    if let Some(port_range) = conf.port_range {
        if port_range.len() == 2 {
            min_port = port_range[0];
            max_port = port_range[1];
        }
    }

    let addr: Vec<&str> = conf.address.split(':').collect();

    let conn_configs = vec![ConnConfig {
        conn,
        relay_addr_generator: Box::new(RelayAddressGeneratorRanges {
            min_port,
            max_port,
            max_retries: 1,
            relay_address: IpAddr::from_str(addr[0])?,
            address: "0.0.0.0".to_owned(),
            net: Arc::new(Net::new(Some(NetConfig::default()))),
        }),
    }];

    let turn_server = TurnServer::new(ServerConfig {
        conn_configs,
        realm: conf.realm,
        auth_handler: new_auth.unwrap(),
        channel_bind_timeout: Duration::from_secs(5),
        alloc_close_notify: None,
    })
    .await?;
    Ok(turn_server)
}
