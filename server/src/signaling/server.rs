use anyhow::Result;
use futures::{Future, StreamExt};
use volcano_sfu::{rtc::{config::{self, WebRTCTransportConfig}, room::Room}, turn};
use std::{fs, pin::Pin, sync::Arc};
use tokio::net::{TcpListener, TcpStream, ToSocketAddrs};

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
pub async fn launch<A: ToSocketAddrs>(addr: A, auth: AuthFn) -> Result<()> {
    // Create TCP listener
    let try_socket = TcpListener::bind(addr).await;
    let listener = try_socket.expect("Failed to bind");

    info!("Server listening on {}", listener.local_addr().unwrap());

    let content = fs::read_to_string("./config.toml")?;
    let c = config::load(&content).inspect_err(|e| error!("Error loading config: {e}. Loading default config.")).unwrap_or_default();
    let config = c.clone();
    if c.turn.enabled {
        turn::init_turn_server(c.turn, c.turn_auth).await?;
    }
    let webrtc_config = Arc::new(WebRTCTransportConfig::new(&config).await?);
    // Accept new connections
    let auth = Arc::new(auth);
    while let Ok((stream, _)) = listener.accept().await {
        tokio::spawn(accept_connection(stream, auth.clone(), Arc::clone(&webrtc_config)));
    }

    Ok(())
}

/// Accept a new TCP connection
async fn accept_connection(stream: TcpStream, auth: Arc<AuthFn>, w: Arc<WebRTCTransportConfig>) {
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
    if let Err(error) = handle_connection((read, write.clone()), auth, w).await {
        write
            .send(PacketS2C::Error {
                error: error.to_string(),
            })
            .await
            .ok();
    }
}

/// Wrap error handling around the connection and authenticate the client
async fn handle_connection((mut read, write): ReadWritePair, auth: Arc<AuthFn>, w: Arc<WebRTCTransportConfig>) -> Result<()> {
    // Wait until valid packet is sent
    let mut client: Option<Client> = None;
    while let Some(msg) = read.next().await {
        if let Some(packet) = PacketC2S::from(msg?)? {
            match packet {
                PacketC2S::Connect { token, room_ids } => {
                    if let Ok(user) = (auth)(token).await {
                        info!("Authenticated user {}", user.id);

                        // Create a new client
                        client = Some(Client::new(user, Arc::clone(&w)).await?);

                        // Fetch rooms
                        let available_rooms = Room::fetch_rooms(&room_ids);

                        // Send inmediate response
                        write.send(PacketS2C::Accept { available_rooms }).await?;
                    }
                    break;
                }
                _ => {}
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
