# Volcano SFU – Rust Selective Forwarding Unit 🚀

## 🧾 Features

- Async-first architecture (`tokio`)
- Callbacks for peer events
- Two peer connections (subscriber and publisher) per room
- Simple integration for WebSockets or other protocols
- API data channel for room events
- Voice activity detection

## 🧪 How to use

### WebSocket example

This example is extracted from [volcano server](https://github.com/MilcaoStudio/volcano/blob/main/server/src/signaling/client.rs)

```rust
    pub async fn handle_join(
        &self,
        write: &Sender,
        room: Arc<Room>,
        initial_offer: RTCSessionDescription,
        cfg: &JoinConfig,
        id: u32,
    ) -> Result<()> {
        let peer = &self.peer;
        let write_out_1 = write.clone();
        let write_out_2 = write.clone();
        let write_out_3 = write.clone();
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
        peer.register_on_ice_connection_state_change(Box::new(move |state| {
            let peer_id_in = peer_id.clone();
            let write_in = write_out_3.clone();
            Box::pin(async move {
                debug!(
                    "[Publisher {}] ICE connection state changed to: {}",
                    peer_id_in, state
                );
                match state {
                    RTCIceConnectionState::Failed => {
                        if let Err(err) = write_in
                            .send(PacketS2C::ServerError {
                                error: ServerError::PeerConnectionFailed,
                            })
                            .await
                        {
                            error!("Write failed: {err}");
                        };
                    }
                    _ => {}
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
                write
                    .send(PacketS2C::Answer {
                        id,
                        description: answer,
                    })
                    .await?;
            }
            Err(err) => {
                // Client should know error
                write
                    .send(PacketS2C::Error {
                        error: err.to_string(),
                    })
                    .await?;
                error!("answer error: {}", err);
            }
        };

        // Send room info
        let room_info = room.get_room_info();
        if let Err(err) = write.send(PacketS2C::RoomInfo { room: room_info }).await {
            error!("send room info error: {}", err);
        };
        Ok(())
    }
```