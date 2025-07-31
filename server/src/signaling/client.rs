use std::sync::Arc;

use anyhow::Result;
use futures::{TryStreamExt};
use volcano_sfu::rtc::{
    config::WebRTCTransportConfig,
    peer::{PeerConfig, PubSubPeer},
    room::Room,
};
use webrtc::{
    ice_transport::{
        ice_candidate::RTCIceCandidateInit, ice_connection_state::RTCIceConnectionState,
    },
    peer_connection::sdp::session_description::RTCSessionDescription,
};

use crate::reference::ReferenceDb;

use super::{
    packets::{PacketC2S, PacketS2C, ServerError},
    sender::{ReadWritePair, Sender},
    server::UserInformation,
};

/// Information about user, room and peer connection
pub struct Client {
    user: UserInformation,
    pub room: Option<Arc<Room>>,
    pub peer: Arc<PubSubPeer>,
    db: Arc<ReferenceDb>,
}

impl Client {
    /// Create a new Client for a user in a room
    pub async fn new(user: UserInformation, config: Arc<WebRTCTransportConfig>, db: Arc<ReferenceDb>) -> Result<Self> {
        Ok(Self {
            db,
            user: user.clone(),
            room: None,
            peer: Arc::new(PubSubPeer::new(user.id.to_owned(), config)),
        })
    }

    /// Run client lifecycle
    pub async fn run(mut self, stream: ReadWritePair) -> Result<()> {
        // Start working
        let result = self.lifecycle_listen(stream).await;

        // Clean up after ourselves
        self.lifecycle_clean_up().await?;

        // Return work result
        result
    }

    /// Listen for incoming packets
    pub async fn lifecycle_listen(&mut self, stream: ReadWritePair) -> Result<()> {
        // Deconstruct read / write pair
        let (mut read, write) = stream;

        debug!("Now accepting incoming messages");

        // Create a worker task for reading WS messages
        async {
            // Read incoming messages
            while let Some(msg) = read.try_next().await? {

                match PacketC2S::from(&msg) {
                    Ok(packet) => {
                        debug!("[Incoming] C->S: {:?}", packet);
                        let result = self.handle_message(packet, &write).await;
                        match result {
                            Ok(_) => debug!("[Incoming] Done!"),
                            Err(e) => error!("[Incoming] Error: {e}"),
                        }
                    }
                    Err(e) => match e {
                        ServerError::UnknownRequest => {
                            write
                                .send(PacketS2C::Error {
                                    error: e.to_string(),
                                })
                                .await?
                        }
                        ServerError::UnproccesableEntity => {
                            debug!(
                                "msg -> {}",
                                msg.into_text().unwrap_or_else(|e| e.to_string())
                            )
                        }
                        _ => {
                            debug!("Websocket message is not a packet.");
                            error!("Error message not handled: {e}");
                        }
                    },
                }
            }

            info!("Websocket worker has finished.");

            Ok(())
        }.await
    }

    /// Clean up after ourselves by disconnecting from the room,
    /// closing the peer connection and removing tracks.
    pub async fn lifecycle_clean_up(&mut self) -> Result<()> {
        let user_id = &self.user.id;
        info!("User {} disconnected", user_id);
        if let Some(room) = &self.room {
            room.remove_peer(user_id).await;
            room.remove_user(user_id).await;
            if room.is_empty() {
                debug!("Room {} is empty. Should clean up?", room.id);
            }
        }
        Ok(())
    }

    /// Handle incoming packet
    async fn handle_message(&mut self, packet: PacketC2S, write: &Sender) -> Result<()> {
        let peer = self.peer.clone();
        match packet {
            PacketC2S::Answer { description } => peer.set_remote_description(description).await,
            PacketC2S::Connect { .. } => write.send(PacketS2C::ServerError { error: ServerError::AlreadyConnected }).await,
            PacketC2S::Continue { .. } => {
                // TODO: Add Continue event
                Ok(())
            }
            PacketC2S::Join {
                id,
                room_id,
                offer,
                cfg,
            } => {
                let room = self.db.fetch_or_create_room(&room_id).await;
                self.room = Some(room.clone());

                self.handle_join(write.clone(), room, offer, &cfg, id).await
            }
            PacketC2S::Leave => {
                match &self.room {
                    Some(room) => {
                        // Close all peers
                        room.remove_peer(&self.user.id).await;
                        // Remove user
                        room.remove_user(&self.user.id).await;
                        if room.is_empty() {
                            room.close().await;
                            self.room = None;
                        }
                        Ok(())
                    }
                    _ => Err(ServerError::RoomNotFound.into()),
                }
            }
            PacketC2S::Remove { removed_tracks: _ } => Ok(()),
            PacketC2S::Offer { id, description } => {
                Self::handle_offer(peer, write.clone(), description, id).await
            }
            PacketC2S::Trickle { candidate, target } => peer.trickle(candidate, target).await,
        }
    }

    pub(super) async fn handle_join(
        &self,
        write: Sender,
        room: Arc<Room>,
        initial_offer: RTCSessionDescription,
        cfg: &PeerConfig,
        id: u32,
    ) -> Result<()> {
        let peer = &self.peer;
        let sender = Arc::new(write);
        let sender_1 = Arc::downgrade(&sender);
        let sender_2 = Arc::downgrade(&sender);
        let sender_3 = Arc::downgrade(&sender);
        peer.on_offer(Box::new(move |offer| {
            let sender_in = sender_1.clone();
            Box::pin(async move {
                if let Some(s) = sender_in.upgrade() {
                    if let Err(err) = s.send(PacketS2C::Offer { description: offer }).await {
                        error!("Send Offer failed: {err}");
                    };
                }
            })
        }))
        .await;

        peer.on_ice_candidate(Box::new(
            move |candidate: RTCIceCandidateInit, target: u8| {
                let sender_in = sender_2.clone();
                Box::pin(async move {
                    if let Some(s) = sender_in.upgrade() {
                        if let Err(err) = s.send(PacketS2C::Trickle { candidate, target })
                            .await
                        {
                            error!("Send Trickle failed: {err}");
                        };
                    }
                })
            },
        ))
        .await;
        let peer_id = peer.id().clone();
        peer.register_on_ice_connection_state_change(Box::new(move |state| {
            let peer_id_in = peer_id.clone();
            let sender_in = sender_3.clone();
            Box::pin(async move {
                debug!(
                    "[Publisher {}] ICE connection state changed to: {}",
                    peer_id_in, state
                );
                if state == RTCIceConnectionState::Failed {
                    if let Some(s) = sender_in.upgrade() {
                        if let Err(err) = s.send(PacketS2C::ServerError { error: ServerError::PeerConnectionFailed, }).await {
                            error!("Send ServerError failed: {err}");
                        };
                    }
                }
            })
        }))
        .await;

        if let Err(err) = peer.join(room.clone(), cfg).await {
            error!("join error: {}", err);
            return Err(err);
        }

        match peer.answer(initial_offer).await {
            Ok(answer) => {
                // Sends back request id
                sender
                    .send(PacketS2C::Answer {
                        id,
                        description: answer,
                    })
                    .await?;
            }
            Err(err) => {
                // Client should know error
                sender
                    .send(PacketS2C::Error {
                        error: err.to_string(),
                    })
                    .await?;
                error!("answer error: {}", err);
            }
        };

        // Set up subscriber... on join?
        if !cfg.no_subscribe {
            info!("[{}] Set up subscriber", self.user.id);
            peer.setup_subscriber(&cfg).await?;

            if !cfg.no_publish {
                if let Some(sub) = peer.subscriber().await {
                    for dc in room.get_data_channel_middlewares().iter() {
                        sub.add_data_channel(&dc.config.label).await?;
                    }
                }
            }

            info!("[Peer {}] Subscribe to room {}", peer.id(), room.id);
            room.subscribe_peer(peer.clone()).await;
        }

        // Send room info
        let room_info = room.get_room_info();
        if let Err(err) = sender.send(PacketS2C::RoomInfo { room: room_info }).await {
            error!("send room info error: {}", err);
        };

        // Listen to room events
        let mut event_rx = room.subscribe_to_events();
        tokio::spawn(async move {
            // Moves room
            while let Ok(event) = event_rx.recv().await {
                debug!("{:?}", event);
                // TODO: send event using subscriber data channel
            }
            debug!("Room event sender closed");
        });

        // End message handle
        Ok(())
    }

    pub(super) async fn handle_offer(
        peer: Arc<PubSubPeer>,
        write: Sender,
        offer: RTCSessionDescription,
        id: u32,
    ) -> Result<()> {
        match peer.answer(offer).await {
            Ok(answer) => {
                write
                    .send(PacketS2C::Answer {
                        id,
                        description: answer,
                    })
                    .await
            }
            Err(err) => {
                error!("answer error: {}", err);
                Err(err)
            }
        }
    }
}
