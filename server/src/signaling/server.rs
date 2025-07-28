use anyhow::Result;
use futures::{Future, StreamExt};
use std::{pin::Pin, sync::Arc};
use tokio::net::{TcpListener, TcpStream};
use volcano_sfu::rtc::config::{Config, PortMap, WebRTCTransportConfig};

use crate::reference::ReferenceDb;

use super::{
    client::Client,
    packets::{PacketC2S, PacketS2C, ServerError},
    sender::{ReadWritePair, Sender},
};

/// User capabilities
#[derive(Default, Debug, Clone, Serialize, PartialEq, Hash, Eq)]
pub struct UserCapabilities {
    pub audio: bool,
    pub video: bool,
    pub screenshare: bool,
}

/// User Information
#[derive(Debug, Clone, Serialize, Hash, PartialEq, Eq)]
pub struct UserInformation {
    pub id: String,
    pub capabilities: UserCapabilities,
}

/// Authentication function
type AuthFn = Box<
    dyn (Fn(String) -> Pin<Box<dyn Future<Output = Result<UserInformation>> + Send + 'static>>)
        + Send
        + Sync,
>;

/// Launch a new signaling server
pub async fn launch_signaling(addr: &str, config: Config, auth: AuthFn) -> Result<()> {
    // Create TCP listener
    let try_socket = TcpListener::bind(addr).await;
    let listener = try_socket.expect(&format!("Failed to bind {}", addr));

    info!("Server listening on {}", listener.local_addr().expect("Server listening on <unknown ip>"));
    
    //if c.turn.enabled {
    //    turn::init_turn_server(c.turn, c.turn_auth).await?;
    //}
    
    let mut webrtc_config = WebRTCTransportConfig::new(&config);
    info!("WebRTC configuration for SFU v{} loaded!", webrtc_config.version);
    match webrtc_config.bind().await {
        Ok(_) => {
            match &webrtc_config.port_map {
                PortMap::Single(port) =>
                info!("UDP Mux network bound to port {port}"),
                PortMap::Range(min, max) => info!("UDP Ephemeral network bound from {min} to {max} ports"),
            }
        },
        Err(err) => error!("Bind failed: {err}"),
    }
    let config_arc = Arc::new(webrtc_config);
    // Accept new connections
    let auth = Arc::new(auth);
    // Create reference db
    let db = Arc::new(ReferenceDb::default());
    while let Ok((stream, _)) = listener.accept().await {
        tokio::spawn(accept_connection(
            stream,
            auth.clone(),
            Arc::clone(&config_arc),
            Arc::clone(&db),
        ));
    }

    Ok(())
}

/// Accept a new TCP connection
async fn accept_connection(stream: TcpStream, auth: Arc<AuthFn>, w: Arc<WebRTCTransportConfig>, db: Arc<ReferenceDb>) {
    // Validate TCP connection
    stream
        .peer_addr()
        .expect("connected streams should have a peer address");

    // Handshake WebSocket connection
    let ws_stream = tokio_tungstenite::accept_async(stream)
        .await
        .expect("Error during the websocket handshake occurred");

    // Prepare the connection for read / write
    let (write, read) = ws_stream.split();
    let write = Sender::new(write);

    // Handle any resulting errors
    if let Err(error) = handle_connection((read, write.clone()), auth, w, db).await {
        error!("Connection ended with error: {error}");
    }
}

/// Wrap error handling around the connection and authenticate the client
async fn handle_connection(
    (mut read, write): ReadWritePair,
    auth: Arc<AuthFn>,
    w: Arc<WebRTCTransportConfig>,
    db: Arc<ReferenceDb>,
) -> Result<()> {
    // Wait until valid packet is sent
    let mut client: Option<Client> = None;
    while let Some(msg) = read.next().await {
        match PacketC2S::from(&msg?) {
            Ok(packet) => match packet {
                PacketC2S::Connect {
                    id,
                    token,
                    room_ids,
                } => {
                    if let Ok(user) = (auth)(token).await {
                        info!("Authenticated user {}", user.id);

                        let user_id = user.id.clone();
                        
                        // Create a new client
                        client = Some(Client::new(user, Arc::clone(&w), db.clone()).await?);

                        // Fetch rooms
                        let available_rooms = db.fetch_available_rooms(&room_ids).await;

                        // Send inmediate response
                        write
                            .send(PacketS2C::Accept {
                                id,
                                user_id,
                                ice_servers: w.configuration.ice_servers.clone(),
                                available_rooms,
                            })
                            .await?;
                    }
                    break;
                }
                _ => {
                    write
                        .send(PacketS2C::ServerError {
                            error: ServerError::NotAuthenticated,
                        })
                        .await?;
                }
            },
            Err(e) => {
                write.send(PacketS2C::ServerError { error: e }).await?;
            }
        }
    }

    // Check if we are authenticated
    if let Some(client) = client {
        // Accept the new client
        client.run((read, write)).await
    } else {
        Err(ServerError::FailedToAuthenticate.into())
    }
}
