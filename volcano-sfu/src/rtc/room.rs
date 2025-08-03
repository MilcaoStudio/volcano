use std::{
    collections::HashMap, fmt::Debug, sync::{
        atomic::{AtomicBool, Ordering}, Arc
    }
};

use dashmap::{DashMap, DashSet, Entry};

use tokio::sync::{broadcast::{self, Receiver, Sender}};
use webrtc::{
    data::data_channel::DataChannel,
    data_channel::{
        RTCDataChannel, data_channel_message::DataChannelMessage,
        data_channel_state::RTCDataChannelState,
    },
    peer_connection::offer_answer_options::RTCOfferOptions,
    track::track_local::{TrackLocal, track_local_static_rtp::TrackLocalStaticRTP},
};

use serde::Serialize;

use crate::track::{receiver::WebRTCReceiver, router::LocalRouter};

use super::peer::PubSubPeer;

#[derive(Debug, Clone, Serialize)]
pub struct UserStream {
    pub id: String,
    pub tracks: Vec<String>,
    pub simulcast: bool,
}

/// Room event which indicates something happened to a peer
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", content = "data")]
#[non_exhaustive]
pub enum RoomEvent {
    RoomCreated(String),
    RoomClosed(String),
    RoomInfo(RoomInfo),
    TracksRemoved {
        removed_tracks: Vec<String>,
        room_id: String,
    },
    VoiceActivity {
        room_id: String,
        stream_ids: Vec<String>,
    },
    UserSpeaking {
        room_id: String,
        uid: String,
        sids: Vec<String>,
    },
    UserJoined {
        room_id: String,
        uid: String,
    },
    TrackAdded {
        room_id: String,
        uid: String,
        track: String,
        stream: UserStream,
    },
    UserLeft {
        room_id: String,
        uid: String,
    },
}

#[derive(Clone, Debug, Serialize)]
pub struct RoomInfo {
    pub id: String,
    pub users: HashMap<String, Vec<UserStream>>,
}

/// Room consisting of clients which can communicate with one another
#[derive(Debug)]
pub struct Room {
    #[allow(dead_code)]
    pub id: String,
    data_channels: Arc<Vec<Arc<DataChannel>>>,
    labels: DashSet<String>,
    /// The room is already closed
    closed: Arc<AtomicBool>,
    event_sender: Sender<RoomEvent>,
    //participants: DashSet<String>,
    //audio_observer: Arc<Mutex<AudioObserver>>,
    //user_tracks: DashMap<String, Vec<String>>,
    user_streams: DashMap<String, Vec<UserStream>>,
    peers: DashMap<String, Arc<PubSubPeer>>,
    tracks: DashMap<String, Arc<TrackLocalStaticRTP>>,
}

impl Room {
    /// Create a new Room and initialise internal channels and maps
    pub fn new(id: String) -> Arc<Self> {
        let (s, _) = broadcast::channel::<RoomEvent>(8);
        
        Arc::new(Room {
            closed: Default::default(),
            data_channels: Default::default(),
            id,
            event_sender: s,
            labels: Default::default(),
            peers: Default::default(),
            //audio_observer: Arc::new(Mutex::new(audio_observer)),
            //user_tracks: Default::default(),
            user_streams: Default::default(),
            tracks: Default::default(),
        })
    }

    /// Stores data channel provided by client
    pub(crate) async fn add_data_channel(self: &Arc<Self>, owner: &str, dc: Arc<RTCDataChannel>) {
        let label = dc.label().to_string();
        let origin = owner.to_owned();
        let room_out = Arc::clone(self);
        let room_out_2 = Arc::clone(self);
        for lbl in self.labels.iter() {
            if lbl.eq(&label) {
                info!(
                    "[Publisher {} -> Room {}] Data channel `{}` already exists, adding listener",
                    owner, self.id, label
                );
                // Adds message listener if user is already registered
                dc.on_message(Box::new(move |msg: DataChannelMessage| {
                    let room_in = room_out.clone();
                    let label_in = label.clone();
                    let origin_in = origin.clone();
                    Box::pin(async move {
                        // Fanout message to room subscribers
                        room_in.fanout_message(origin_in, label_in, msg).await;
                    })
                }));
                return;
            }
        }

        self.labels.insert(label.clone());

        let peer_owner = self.get_peer(owner).await.unwrap();
        if let Some(subscriber) = peer_owner.subscriber().await {
            subscriber
                .register_data_channel(label.clone(), dc.clone())
                .await;
        }

        let label_out_1 = label.clone();
        let label_out_2 = label.clone();

        dc.on_message(Box::new(move |msg: DataChannelMessage| {
            let room_in = room_out.clone();
            let label_in = label_out_1.clone();
            let origin_in = origin.clone();
            info!("Data channel {} message intercepted", label_in);
            Box::pin(async move {
                // Fanout message to room subscribers
                room_in.fanout_message(origin_in, label_in, msg).await;
            })
        }));

        for peer in self.peers.iter() {
            if peer.id().as_str() == owner || peer.subscriber().await.is_none() {
                continue;
            }

            let subscriber = peer.subscriber().await.unwrap();
            let sub_out = subscriber.clone();
            let room_in = room_out_2.clone();
            let label_out = label_out_2.clone();

            // Creates data channel in subscriber peer
            match subscriber.create_data_channel(label_out.clone()).await {
                Ok(channel) => {
                    channel.on_message(Box::new(move |msg| {
                        let origin = sub_out.id.clone();
                        let room_inner = room_in.clone();
                        let label_in = label_out.clone();
                        Box::pin(async move {
                            info!("Subscriber data channel message intercepted");
                            // Fanout message to room subscribers
                            room_inner.fanout_message(origin, label_in, msg).await;
                        })
                    }))
                }
                _ => {
                    continue;
                }
            }

            info!("Data channel negotiation");
            match subscriber
                .negotiate(Some(RTCOfferOptions {
                    ice_restart: true,
                    voice_activity_detection: true,
                }))
                .await
            {
                Err(err) => {
                    error!("negotiate error:{}", err);
                }
                _ => {
                    info!("Data channel negotiation successful");
                }
            }
        }
    }

    pub async fn add_peer(&self, peer: Arc<PubSubPeer>) {
        let id = peer.id();
        self.peers.insert(id, peer);
    }

    pub async fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
    }

    pub fn get_data_channel_middlewares(&self) -> Arc<Vec<Arc<DataChannel>>> {
        self.data_channels.clone()
    }

    async fn get_data_channels(&self, origin: &str, label: &String) -> Vec<Arc<RTCDataChannel>> {
        let mut data_channels: Vec<Arc<RTCDataChannel>> = Vec::new();

        let peers = self.peers.clone();
        for (k, v) in peers.into_read_only().iter() {
            if k.as_str() == origin {
                info!("[get_data_channels] Skipped peer owner");
                continue;
            }

            if let Some(subscriber) = v.subscriber().await {
                if let Some(dc) = subscriber.data_channel(label).await {
                    if dc.ready_state() == RTCDataChannelState::Open {
                        data_channels.push(dc);
                    }
                }
            }
        }

        data_channels
    }

    pub async fn get_peer(&self, peer_id: &str) -> Option<Arc<PubSubPeer>> {
        self.peers.get(peer_id).map(|peer| peer.clone())
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    pub fn is_empty(&self) -> bool {
        self.user_streams.len() == 0
    }

    /// Fanouts raw message received from any data channel to subscribed data channels
    async fn fanout_message(&self, origin: String, label: String, msg: DataChannelMessage) {
        info!(
            "Message from data channel {}: {:?}",
            self.id,
            msg.data.to_ascii_lowercase()
        );
        let s = match core::str::from_utf8(msg.data.as_ref()) {
            Ok(v) => v,
            Err(e) => {
                error!("Invalid UTF-8 sequence: {}", e);
                ""
            }
        };
        for dc in &self.get_data_channels(&origin, &label).await {
            if let Err(err) = if msg.is_string {
                info!("Message forwarded to {}: {}", dc.label(), s);
                dc.send_text(s).await
            } else {
                info!("Message forwarded to {}: <binary>", dc.label());
                dc.send(&msg.data).await
            } {
                error!("publish_message send error:{}", err);
            }
        }
    }

    /// Listen for events from the room
    pub fn subscribe_to_events(&self) -> Receiver<RoomEvent> {
        debug!("[Room {}] Added subscription to events", self.id);
        self.event_sender.subscribe()
    }

    pub async fn remove_peer(&self, peer_id: &str) -> usize {
        if let Some((_, peer)) = self.peers.remove(peer_id) {
            peer.clean_up().await;
            debug!("peer: {} strong references left", Arc::strong_count(&peer));
        }; // Drop peer

        self.peers.len()
    }

    /// Get all user IDs currently in the room
    pub fn get_user_ids(&self) -> Vec<String> {
        self.user_streams
            .iter()
            .map(|item| item.key().to_owned())
            .collect()
    }

    /// Check if a user is in a room
    pub fn in_room(&self, id: &str) -> bool {
        self.user_streams.contains_key(id)
    }

    /// Adds an user into this room and triggers [RoomEvent::UserJoined]
    pub fn add_user(&self, user_id: String) {

        let room_id = self.id.clone();
        let uid = user_id.clone();

        let ev = RoomEvent::UserJoined { room_id, uid };
        
        if self.user_streams.len() > 0 {
            self.trigger_event(ev);
        }
        
        self.user_streams.insert(user_id, Vec::default());
    }


    /// Adds a track for the given user, looking up for existing streams and pushing
    pub fn add_user_track(&self, user_id: String, stream_id: String, track_id: String, simulcast: bool) {

        let track_id_1 = track_id.clone();
        let updated = match self.user_streams.entry(user_id.clone()) {
            Entry::Occupied(mut entry) => {
                let streams = entry.get_mut();
                let existing = streams.iter_mut().find(|s| s.id == stream_id);
                match existing {
                    // Case 1: Exists stream with same id, push track id in stream
                    Some(stream) => {
                        stream.tracks.push(track_id_1);
                        stream.clone()
                    },
                    // Case 2: Does not exist stream with that id, push stream
                    _ => {
                        let new_stream = UserStream {
                            id: stream_id,
                            tracks: vec![track_id_1],
                            simulcast,
                        };
                        streams.push(new_stream.clone());
                        new_stream
                    }
                }
            },
            Entry::Vacant(entry) => {
                // Case 3: Does not exist user (very rare), insert them
                let s = UserStream {
                    id: stream_id,
                    tracks: vec![track_id_1],
                    simulcast,
                };
                entry.insert(vec![s.clone()]);
                s
            }
        };
        
        let ev = RoomEvent::TrackAdded { room_id: self.id.clone(), uid: user_id, track: track_id, stream: updated };

        self.trigger_event(ev);
    }

    pub fn get_room_info(&self) -> RoomInfo {
        let streams = self.user_streams.clone();
        let mut users = HashMap::new();
        // Serialize user tracks
        streams.into_iter().for_each(|(key, value)| {
            users.insert(key, value);
        });
        RoomInfo {
            id: self.id.clone(),
            users,
        }
    }

    pub async fn subscribe_peer(&self, peer: Arc<PubSubPeer>) {
        // Removed massive data channel creation
        

        if let Some(publisher) = peer.publisher().await {
            publisher.router().start_audio_observer_task().await;
        }

        for cur_peer in self.peers.iter() {
            let cur_id = cur_peer.id();
            let peer_id = peer.id();
            if cur_id == peer_id {
                continue;
            }

            if let Some(p) = cur_peer.publisher().await {
                info!(
                    "[Room {}] Peer {} subscribes to tracks from router {}. No receivers.",
                    self.id, peer_id, cur_id
                );

                let current_router = p.router();
                if let Some(sub) = peer.subscriber().await {
                    // Negotiation is required, despite of add_down_tracks returns Ok(false)
                    if current_router
                        .add_down_tracks(sub.clone(), None)
                        .await
                        .is_err()
                    {
                        continue;
                    }
                } else {
                    warn!(
                        "[Room {}] Expected peer {} subscriber. Got None.",
                        self.id, peer_id
                    );
                }
            }
        }

        info!("Subscribe Negotiate");
        if let Err(err) = peer.subscriber().await.unwrap().negotiate(None).await {
            error!("negotiate error: {}", err);
        }
    }
    /// Remove a user from the room
    pub async fn remove_user(&self, id: &str) {
        // Find all associated track information
        if let Some((_, streams)) = self.user_streams.remove(id) {
            for stream in &streams {
                for track_id in &stream.tracks {
                    self.close_track(track_id);
                }
            }

            // Client should remove tracks on user left event
        }

        if !self.user_streams.is_empty() {
            // Let everyone know we left
            self.trigger_event(RoomEvent::UserLeft {
                room_id: self.id.clone(),
                uid: id.to_owned(),
            });
        }
    }

    pub fn trigger_event(&self, event: RoomEvent) {
        let id = &self.id;
        match self.event_sender.send(event) {
            Ok(count) => {
                debug!("[Room {id}] Event sent to {count} listeners");
            },
            Err(err) => {
                error!("[Room {id}] Send event failed: {err}");
            }
        }
    }

    /// Add a local track
    pub async fn add_track(&self, user_id: String, local_track: Arc<TrackLocalStaticRTP>) {
        let id = local_track.id().to_owned();
        info!("{user_id} started broadcasting track with ID {id} to all users");

        self.tracks.insert(id.to_owned(), local_track);
    }

    /// Get a local track
    pub fn get_track(&self, id: &str) -> Option<Arc<TrackLocalStaticRTP>> {
        self.tracks.get(id).map(|value| value.clone())
    }

    /// Remove a local track
    pub async fn remove_track(&self, id: String) {
        self.close_track(&id);

        self.send_to_subscribers(RoomEvent::TracksRemoved {
            removed_tracks: vec![id],
            room_id: self.id.clone(),
        })
        .await;
    }

    pub async fn publish_track(
        &self,
        router: &LocalRouter,
        receiver: Arc<WebRTCReceiver>,
    ) {
        for peer in self.peers.iter() {
            let peer_id = peer.id();
            // no subscriber or same id = no publish
            if router.id() == peer_id {
                info!("Same id ({peer_id}) skipped.");
                continue;
            }

            let Some(sub) = peer.subscriber().await else {
                warn!(
                    "[Room {}] Expected peer {} subscriber. Got None.",
                    self.id, peer_id
                );
                continue;
            };

            info!("Publishing track to peer subsriber, peer_id: {}", peer_id);
            match router
                .add_down_tracks(sub.clone(), Some(receiver.clone()))
                .await
            {
                Ok(negotiate) => {
                    if negotiate {
                        if let Err(err) = sub.negotiate(None).await {
                            warn!(
                                "[Room {}] Peer {} negotiate error: {}",
                                self.id, peer_id, err
                            );
                        };
                    }
                }
                Err(err) => {
                    warn!(
                        "[Room {}] Publish track to peer {} failed: {}",
                        self.id, peer_id, err
                    );
                    continue;
                }
            }
        }
    }

    /// Close local track
    fn close_track(&self, id: &str) {
        info!("Track {id} has been removed");
        self.tracks.remove(id);

        // Router stops when peers are closed
    }

    /// Sends a serializable message to all peers' subscribers
    pub async fn send_to_subscribers<Message>(&self, msg: Message)
    where
        Message: Serialize + Debug,
    {
        if let Ok(payload) = serde_json::to_string(&msg) {
            for peer in self.peers.iter() {
                match peer.subscriber().await {
                    Some(subscriber) => {
                        subscriber.send_message(&payload).await;
                    }
                    _ => {
                        warn!("[{}] No subscriber available", peer.id());
                    }
                }
            }
        } else {
            error!("Error parsing {:?}", msg);
        };
    }
}