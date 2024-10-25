use std::sync::Arc;

use anyhow::Result;
use futures::{
    future::{select, Either}, pin_mut, FutureExt, TryStreamExt
};
use postage::stream::Stream;
use webrtc::{
    ice_transport::ice_candidate::RTCIceCandidateInit,
    peer_connection::sdp::session_description::RTCSessionDescription,
};

use volcano_sfu::rtc::{
    config::WebRTCTransportConfig,
    peer::{JoinConfig, Peer},
    room::{Room, RoomEvent, RoomSignal},
};

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
                if let Some(msg) = PacketC2S::from(msg)? {
                    self.handle_message(msg, &write).await?;
                } else {
                    write
                        .send(PacketS2C::Error {
                            error: ServerError::UnknownRequest.to_string(),
                        })
                        .await?;
                }
            }

            Ok(())
        }
        .fuse();
    
        let room_worker = async {
            debug!("Created room listener");
            let mut listener = signal.listener();
            // Read incoming events
            while let Some(event) = listener.recv().await {
                match event {
                    RoomEvent::Create(id) => {
                        write
                            .send(PacketS2C::RoomCreate { id, owner: None })
                            .await?;
                    }
                    RoomEvent::DataChannelMessage(msg) => match String::from_utf8(msg) {
                        Ok(str) => write.send(PacketS2C::Message(str)).await?,
                        Err(e) => error!("Invalid UTF-8 sequence: {}", e),
                    },
                    RoomEvent::RelayPeerRequest { payload, room_id } => {
                        write.send(PacketS2C::RelayRequest { payload, room_id }).await?;
                    }
                    RoomEvent::UserJoin { room_id, user_id, .. } => {
                        write
                            .send(PacketS2C::UserJoin {
                                room_id,
                                user_id,
                            })
                            .await?;
                    }
                    RoomEvent::Close(id) => {
                        write.send(PacketS2C::RoomDelete { id }).await?;
                    },
                    _=>{}
                }
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
        debug!("C->S: {:?}", packet);

        let peer = self.peer.clone();
        match packet {
            PacketC2S::Answer { description } => peer.set_remote_description(description).await,
            PacketC2S::Connect { .. } => Err(ServerError::AlreadyConnected.into()),
            PacketC2S::Continue { .. } => {

                // TODO: Add Continue event
                Ok(())
            }
            PacketC2S::Join {
                room_id,
                offer,
                cfg,
            } => {
                let room = Room::get(&room_id);
                self.room = Some(room.clone());
                self.handle_join(write, room.clone(), offer, cfg).await
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
            PacketC2S::Offer { description } => {
                Self::handle_offer(peer, write.clone(), description).await
            }
            PacketC2S::Trickle { candidate, target } => {
                info!("Incoming tricke for {target}");
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
    ) -> Result<()> {

        room.subscribe_signal(self.signal.clone()).await;
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

        if let Err(err) = peer.join(room.id.clone(), cfg).await {
            error!("join error: {}", err);
            return Err(err);
        }

        match peer.answer(initial_offer).await {
            Ok(answer) => {
                write
                    .send(PacketS2C::Answer {
                        description: answer,
                    })
                    .await?;
            }
            Err(err) => {
                error!("answer error: {}", err);
            }
        };
        Ok(())
    }

    pub(super) async fn handle_offer(
        peer: Arc<Peer>,
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
                return Err(err);
            }
        }
    }
}
