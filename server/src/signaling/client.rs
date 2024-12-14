use std::sync::Arc;

use anyhow::Result;
use futures::{
    future::{select, Either}, pin_mut, FutureExt, TryStreamExt
};
use postage::stream::Stream;
use volcano_sfu::rtc::{
    config::WebRTCTransportConfig,
    peer::{JoinConfig, Peer},
    room::{Room, RoomSignal},
};
use webrtc::{ice_transport::ice_candidate::RTCIceCandidateInit, peer_connection::sdp::session_description::RTCSessionDescription};

use super::{
    packets::{PacketC2S, PacketS2C, ServerError},
    sender::{ReadWritePair, Sender},
    server::UserInformation,
};

/// Information about user, room and peer connection
pub struct Client {
    user: UserInformation,
    pub room: Option<Arc<Room>>,
    pub signal: Arc<RoomSignal>,
    pub peer: Arc<Peer>,
}

impl Client {
    /// Create a new Client for a user in a room
    pub async fn new(user: UserInformation, config: Arc<WebRTCTransportConfig>) -> Result<Self> {
        Ok(Self {
            user: user.clone(),
            room: None,
            signal: RoomSignal::new(Some(user.id.to_owned())),
            peer: Arc::new(Peer::new(user.id.to_owned(), config).await?),
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

        debug!("Now accepting incoming messages and room events");

        let signal = self.signal.clone();
        // Create a worker task for reading WS messages
        let ws_worker = async {
            // Read incoming messages
            while let Some(msg) = read.try_next().await? {
                debug!("Websocket message received.");
                match PacketC2S::from(msg) {
                    Ok(packet) => self.handle_message(packet, &write).await?,
                    Err(e) => {
                        match e {
                            ServerError::UnknownRequest => write
                            .send(PacketS2C::Error {
                                error: e.to_string(),
                            })
                            .await?,
                            _ => {
                                debug!("Websocket message is not a packet.");
                                error!("Error message not handled: {e}");
                            }
                        }
                    }
                }
            }

            info!("Websocket worker has finished.");

            Ok(())
        }
        .fuse();
    
        let room_worker = async {
            debug!("Created room listener");
            let mut listener = signal.listener();
            // Read incoming events
            while let Some(event) = listener.recv().await {
                warn!("Room listener was experimental. Events might not operate as expected");
                info!("Room event: {event:?}");
            }

            // TODO: maybe throw an error for listener being closed?
            info!("Closing room listener");
            anyhow::Ok(())
        }.fuse();

        // Pin futures on the stack
        pin_mut!(ws_worker, room_worker);

        match select(ws_worker, room_worker).await {
            Either::Left((result, _)) => result,
            Either::Right((result, _)) => result,
        }
    }

    /// Clean up after ourselves by disconnecting from the room,
    /// closing the peer connection and removing tracks.
    pub async fn lifecycle_clean_up(&mut self) -> Result<()> {
        info!("User {} disconnected", self.user.id);
        if let Some(room) = &self.room {
            room.unsubscribe_signal(&self.user.id).await;
            room.remove_user(&self.user.id).await;
            if room.is_empty() {
                room.close().await;
            }
        }
        self.peer.clean_up().await
    }

    /// Handle incoming packet
    async fn handle_message(&mut self, packet: PacketC2S, write: &Sender) -> Result<()> {
        info!("C->S: {:?}", packet);

        let peer = self.peer.clone();
        match packet {
            PacketC2S::Answer { description } => peer.set_remote_description(description).await,
            PacketC2S::Connect { .. } => Err(ServerError::AlreadyConnected.into()),
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
                let room = Room::get(&room_id);
                self.room = Some(room.clone());
                self.handle_join(write, room.clone(), offer, cfg, id).await
            }
            PacketC2S::Leave => {
                match &self.room {
                    Some(room) => {
                        // Close all peers
                        self.peer.clean_up().await?;
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
            PacketC2S::Offer {id, description } => {
                Self::handle_offer(peer, write.clone(), description, id).await
            }
            PacketC2S::Trickle { candidate, target } => {
                peer.trickle(candidate, target).await
            }
        }
    }

    pub(super) async fn handle_join(
        &self,
        write: &Sender,
        room: Arc<Room>,
        initial_offer: RTCSessionDescription,
        cfg: JoinConfig,
        id: u32,
    ) -> Result<()> {

        // Signaling was experimental.
        // room.subscribe_signal(self.signal.clone()).await;
        let peer = self.peer.clone();
        let write_out_1 = write.clone();
        let write_out_2 = write.clone();
        peer.on_offer(Box::new(move |offer| {
            let write_in = write_out_1.clone();
            Box::pin(async move {
                if let Err(err) = write_in.send(PacketS2C::Offer { description: offer }).await {
                    error!("on_offer error: {err}");
                };
            })
        }))
        .await;

        peer.on_ice_candidate(Box::new(
            move |candidate: RTCIceCandidateInit, target: u8| {
                let write_in = write_out_2.clone();
                Box::pin(async move {
                    if let Err(err) = write_in
                        .send(PacketS2C::Trickle { candidate, target })
                        .await
                    {
                        error!("on_ice_candidate error: {err}");
                    };
                })
            },
        ))
        .await;
        let peer_id = peer.id().clone();
        peer.register_on_ice_connection_state_change(Box::new( move |state| {
            let peer_id_in = peer_id.clone();
            Box::pin(async move {
                debug!("[Publisher {}] ICE connection state changed to: {}", peer_id_in, state);
            })
        })).await;

        if let Err(err) = peer.join(room.id.clone(), cfg).await {
            error!("join error: {}", err);
            return Err(err);
        }

        match peer.answer(initial_offer).await {
            Ok(answer) => {
                // Sends back request id
                write
                    .send(PacketS2C::Answer {
                        id,
                        description: answer,
                    })
                    .await?;
            }
            Err(err) => {
                // Client should know error
                write.send(PacketS2C::Error { error: err.to_string() }).await?;
                error!("answer error: {}", err);
            }
        };
        Ok(())
    }

    pub(super) async fn handle_offer(
        peer: Arc<Peer>,
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
                return Err(err);
            }
        }
    }
}
