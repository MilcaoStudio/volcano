use std::sync::Arc;

use anyhow::Result;
use futures::{TryStreamExt};
use tokio::sync::{broadcast::error::RecvError, watch};
use volcano_sfu::rtc::{
    config::WebRTCTransportConfig,
    peer::{PeerConfig, PeerRole, PubSubPeer},
    room::Room,
};
use webrtc::{
    ice_transport::{
        ice_candidate::RTCIceCandidateInit,
    },
    peer_connection::sdp::session_description::RTCSessionDescription,
};

use crate::{reference::ReferenceDb, signaling::packets::{BadRequestError, LostConnectionError}};

use super::{
    packets::{PacketC2S, PacketS2C, ServerError},
    sender::{ReadWritePair, Sender},
    server::UserInformation,
};

/// Information about user, room and peer connection
pub struct Client {
    close_events_tx: watch::Sender<bool>,
    close_events_rx: watch::Receiver<bool>,
    user: UserInformation,
    pub room: Option<Arc<Room>>,
    pub peer: Arc<PubSubPeer>,
    db: Arc<ReferenceDb>,
}

impl Client {
    /// Create a new Client for a user in a room
    pub fn new(user: UserInformation, config: Arc<WebRTCTransportConfig>, db: Arc<ReferenceDb>) -> Self {
        let (close_tx, close_rx) = watch::channel(false);
        Self {
            close_events_tx: close_tx,
            close_events_rx: close_rx,
            db,
            user: user.clone(),
            room: None,
            peer: Arc::new(PubSubPeer::new(user.id.to_owned(), config)),
        }
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
                        BadRequestError::BadFormat { reason } => {
                            write
                                .send(BadRequestError::BadFormat { reason })
                                .await?
                        }
                        BadRequestError::UnproccesableEntity => {
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
            if let Err(_) = self.close_events_tx.send(true) {
                warn!("[Client {0}] Send close signal failed.", user_id);
            };
        
        Ok(())
    }

    /// Handle incoming packet
    async fn handle_message(&mut self, packet: PacketC2S, write: &Sender) -> Result<()> {
        let peer = self.peer.clone();
        match packet {
            PacketC2S::Answer { description } => {
                if let Err(err) = peer.set_remote_description(description).await {
                    error!("[Client {0}] [On Answer] Set remote description failed: {err}", self.user.id);
                    return write.send(LostConnectionError::SubscriberConnectionLost).await;
                }
                Ok(())
            },
            PacketC2S::Connect { .. } => write.send(BadRequestError::AlreadyConnected).await,
            PacketC2S::Join {
                room_id,
                offer,
                cfg,
            } => {
                // Send "open" without receivers
                self.close_events_tx.send_replace(false);
                let room = self.db.fetch_or_create_room(&room_id).await;
                self.room = Some(room.clone());

                self.handle_join(write.clone(), room, offer, &cfg).await
            }
            PacketC2S::Leave => {
                match self.room.take() {
                    Some(room) => {
                        // Close all peers
                        room.remove_peer(&self.user.id).await;
                        // Remove user
                        room.remove_user(&self.user.id).await;
                        if room.is_empty() {
                            room.close().await;
                        }
                        let _ = self.close_events_tx.send(true);
                        Ok(()) // Drop room
                    }
                    _ => Err(ServerError::RoomNotFound.into()),
                }
            }
            PacketC2S::Offer { description } => {
                Self::handle_offer(peer, write.clone(), description).await
            }
            PacketC2S::Ping { data } => write.send(PacketS2C::Pong { data }).await,
            PacketC2S::Trickle { candidate, target } => {
                if let Err(err) = peer.trickle(candidate, target).await {
                    error!("[Client {0}] [On Trickle] Add candidate failed: {err}", self.user.id);
                    return write.send(LostConnectionError::PeerConnectionLost).await;
                };
                Ok(())
            },
        }
    }

    pub(super) async fn handle_join(
        &self,
        write: Sender,
        room: Arc<Room>,
        initial_offer: Option<RTCSessionDescription>,
        cfg: &PeerConfig,
    ) -> Result<()> {
        let peer = &self.peer;
        let sender = Arc::new(write);
        let sender_1 = sender.clone();
        let sender_2 = sender.clone();
        //let sender_3 = Arc::downgrade(&sender);
        peer.on_offer(Box::new(move |offer| {
            let sender_in = sender_1.clone();
            Box::pin(async move {
                if let Err(err) = sender_in.send(PacketS2C::Offer { description: offer }).await {
                    error!("Send Offer failed: {err}");
                }
            })
        }))
        .await;

        peer.on_ice_candidate(Box::new(
            move |candidate: RTCIceCandidateInit, target: PeerRole| {
                let sender_in = sender_2.clone();
                Box::pin(async move {
                    if let Err(err) = sender_in.send(PacketS2C::Trickle { candidate, target })
                        .await {
                        error!("Send Trickle failed: {err}");
                    };
                    
                })
            },
        ))
        .await;
        let peer_id = peer.id().clone();
        peer.register_on_ice_connection_state_change(Box::new(move |state| {
            let peer_id_in = peer_id.clone();
            Box::pin(async move {
                debug!(
                    "[Publisher {}] ICE connection state changed to: {}",
                    peer_id_in, state
                );
                // Do nothing
            })
        }))
        .await;

        if let Err(err) = peer.join(room.clone(), cfg).await {
            error!("join error: {}", err);
            return Err(err.into());
        }

        if let Some(offer) = initial_offer {
            match peer.answer(offer).await {
                Ok(answer) => {
                    // Sends back request id
                    sender
                        .send(PacketS2C::Answer {
                            description: answer,
                        })
                        .await?;
                }
                Err(err) => {
                    error!("answer error: {}", err);
                    // Client should know error
                    sender
                        .send(LostConnectionError::PublisherConnectionLost)
                        .await?;
                }
            };
        }

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

        let mut close_rx = self.close_events_rx.clone();
        tokio::spawn(async move {
            let mut event_rx = room.subscribe_to_events();
            loop {
                // Listen to room events
                tokio::select! {
                    result = event_rx.recv() => {
                        match result {
                            Ok(event) => {
                                room.send_to_subscribers(event).await;
                            }
                            Err(RecvError::Lagged(n)) => {
                                warn!("Room event listener lagged. {n} events.");
                            }
                            Err(RecvError::Closed) => {
                                break
                            }
                        }
                    }

                    msg = close_rx.changed() => {
                        match msg {
                            Ok(_) => {
                                if *close_rx.borrow_and_update() {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                }
            }
            debug!("End task: Listen to room events");
        });

        // End message handle
        Ok(())
    }

    pub(super) async fn handle_offer(
        peer: Arc<PubSubPeer>,
        write: Sender,
        offer: RTCSessionDescription,
    ) -> Result<()> {
        match peer.answer(offer).await {
            Ok(answer) => {
                write
                    .send(PacketS2C::Answer {
                        description: answer,
                    })
                    .await
            }
            Err(err) => {
                error!("answer error: {}", err);
                write.send(LostConnectionError::PublisherConnectionLost).await
            }
        }
    }
}
