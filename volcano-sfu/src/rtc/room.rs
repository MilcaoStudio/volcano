use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use dashmap::{DashMap, DashSet};
use postage::{
    broadcast::{channel, Receiver, Sender},
    sink::Sink,
};
use tokio::sync::Mutex;
use ulid::Ulid;
use webrtc::{
    data::data_channel::DataChannel,
    data_channel::{
        data_channel_message::DataChannelMessage, data_channel_state::RTCDataChannelState,
        RTCDataChannel,
    },
    track::track_local::{track_local_static_rtp::TrackLocalStaticRTP, TrackLocal},
};

use crate::track::{audio_observer::AudioObserver, router::LocalRouter};

use super::peer::Peer;

/// Room event which indicates something happened to a peer
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RoomEvent {
    Create(String),
    Close(String),
    RelayPeerRequest {
        payload: String,
        room_id: String,
    },
    DataChannelMessage(Vec<u8>),
    RemoveTrack {
        removed_tracks: Vec<String>,
        room: String,
    },
    UserJoin {
        room_id: String,
        user_id: String,
        user_tracks: Vec<String>,
    },
    UserLeft {
        room_id: String,
        user_id: String,
    },
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
    /// Signalers for this room
    signalers: Arc<Mutex<Vec<Arc<RoomSignal>>>>,
    sender: Sender<RoomEvent>,
    participants: DashSet<String>,
    pub audio_observer: Arc<Mutex<AudioObserver>>,
    user_tracks: DashMap<String, Vec<String>>,
    peers: DashMap<String, Arc<Peer>>,
    tracks: DashMap<String, Arc<TrackLocalStaticRTP>>,
}
#[derive(Debug, Serialize)]
pub struct RoomInfo {
    id: String,
    participants: Vec<String>,
}

lazy_static! {
    static ref ROOMS: DashMap<String, Arc<Room>> = DashMap::new();
}

impl Room {
    /// Create a new Room and initialise internal channels and maps
    fn new(id: String) -> Arc<Self> {
        let (sender, _dropped) = channel(10);

        Arc::new(Room {
            closed: Default::default(),
            data_channels: Default::default(),
            id: id.clone(),
            sender,
            signalers: Default::default(),
            labels: Default::default(),
            peers: Default::default(),
            participants: Default::default(),
            audio_observer: Arc::new(Mutex::new(AudioObserver::new(100, 80, 50))),
            user_tracks: Default::default(),
            tracks: Default::default(),
        })
    }

    /// Stores data channel provided by client
    pub(crate) async fn add_data_channel(self: &Arc<Self>, owner: &str, dc: Arc<RTCDataChannel>) {
        let label = dc.label().to_string();
        let origin = owner.to_owned();
        let room_out = Arc::clone(&self);
        let room_out_2 = Arc::clone(&self);
        for lbl in self.labels.iter() {
            if lbl.eq(&label) {
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
            if let Ok(channel) = subscriber.create_data_channel(label_out.clone()).await {
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
            } else {
                continue;
            }

            info!("Data channel negotiation");
            if let Err(err) = subscriber.negotiate().await {
                error!("negotiate error:{}", err);
            } else {
                info!("Data channel negotiation successful");
            }
        }
    }

    pub async fn add_peer(&self, peer: Arc<Peer>) {
        let id = peer.id();
        info!("Peer {} added to room {}", &id, self.id);
        self.peers.insert(id, peer);
    }

    pub async fn subscribe_signal(&self, signal: Arc<RoomSignal>) {
        info!("Signaler {} subscribed to room {}", signal.id, self.id);
        self.signalers.lock().await.push(signal);
    }

    pub async fn unsubscribe_signal(&self, id: &str) {
        self.signalers.lock().await.retain(|s| s.id != id);
    }

    pub async fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
        self.publish(RoomEvent::Close(self.id.clone())).await;
        self.signalers.lock().await.clear();
        //let mut close_handler = self.on_close_handler.lock().await;
        //if let Some(f) = &mut *close_handler {
        //    f().await;
        //}
    }

    /// Get or create a Room by its ID
    pub fn get(id: &str) -> Arc<Room> {
        if let Some(room) = ROOMS.get(id) {
            room.clone()
        } else {
            let room: Arc<Room> = Room::new(id.to_owned());
            ROOMS.insert(id.to_string(), room.clone());

            room
        }
    }

    pub fn fetch_rooms(ids: &Vec<String>) -> Vec<RoomInfo> {
        let mut available_rooms = Vec::new();
        for id in ids {
            if let Some(room) = ROOMS.get(id) {
                available_rooms.push(RoomInfo {
                    id: id.clone(),
                    participants: room.participants.clone().into_iter().collect(),
                })
            }
        }
        available_rooms
    }

    pub(super) fn get_data_channel_middlewares(&self) -> Arc<Vec<Arc<DataChannel>>> {
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

    pub async fn get_peer(&self, peer_id: &str) -> Option<Arc<Peer>> {
        self.peers.get(peer_id).map(|peer| peer.clone())
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    pub fn is_empty(&self) -> bool {
        self.user_tracks.len() == 0
    }

    /// Publish an event to the room
    pub async fn publish(&self, event: RoomEvent) {
        info!("Room {} emitted {:?}", self.id, event);
        //self.sender.clone().try_send(event).ok();
        for signal in &*self.signalers.lock().await {
            signal.publish(&self.id, event.clone());
        }
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
                dc.send(&msg.data).await
            } {
                error!("publish_message send error:{}", err);
            }
        }
    }

    /// Listen for events from the room
    pub fn listener(&self) -> Receiver<RoomEvent> {
        self.sender.subscribe()
    }

    pub async fn remove_peer(&self, peer_id: &str) -> usize {
        self.peers.remove(peer_id);

        let peer_count = self.peers.len();
        peer_count
    }

    /// Get all user IDs currently in the room
    pub fn get_user_ids(&self) -> Vec<String> {
        self.user_tracks
            .iter()
            .map(|item| item.key().to_owned())
            .collect()
    }

    /// Check if a user is in a room
    pub fn in_room(&self, id: &str) -> bool {
        self.user_tracks.contains_key(id)
    }

    /// Join a new user into the room
    pub async fn join_user(&self, id: String, tracks: Vec<String>) {
        let ev = RoomEvent::UserJoin {
            room_id: self.id.clone(),
            user_id: id.clone(),
            user_tracks: tracks.clone(),
        };
        self.send_room_event(ev).await;

        // Add tracks to map
        self.user_tracks.insert(id, tracks);
    }

    pub async fn subscribe(self: &Arc<Self>, peer: Arc<Peer>) {
        info!("Subscribing a new peer");

        // Subscriber data channels to peer subscriber
        for label in self.labels.iter() {
            let room_out = self.clone();
            let lbl_out = label.clone();
            if let Some(subscriber) = peer.subscriber().await {
                let sub_out = subscriber.clone();
                match subscriber.create_data_channel(label.clone()).await {
                    Ok(dc) => dc.on_message(Box::new(move |msg| {
                        let room_in = room_out.clone();
                        let origin = sub_out.clone().id.clone();
                        let lbl_in = lbl_out.clone();
                        Box::pin(async move {
                            room_in.fanout_message(origin, lbl_in, msg).await;
                        })
                    })),
                    _ => continue,
                }
            }
        }

        for cur_peer in self.peers.iter() {
            let cur_id = cur_peer.id();
            let peer_id = peer.id();
            let publisher = cur_peer.publisher().await;
            if cur_id == peer_id || publisher.is_none() {
                continue;
            }

            info!(
                "PeerLocal Subscribe to publisher streams , cur_peer_id:{} , peer_id:{}",
                cur_id, peer_id
            );

            let p = publisher.unwrap();
            let router = p.router();
            if router
                .add_down_tracks(peer.subscriber().await.unwrap(), None)
                .await
                .is_err()
            {
                continue;
            }

            info!("Subscribe Negotiate");
            if let Err(err) = peer.subscriber().await.unwrap().negotiate().await {
                error!("negotiate error: {}", err);
            }
        }
    }
    /// Remove a user from the room
    pub async fn remove_user(&self, id: &str) {
        // Find all associated track information
        if let Some((_, tracks)) = self.user_tracks.remove(id) {
            let removed_tracks = tracks.clone();

            // Release Mutex lock
            drop(tracks);

            for id in &removed_tracks {
                self.close_track(id);
            }

            self.publish(RoomEvent::RemoveTrack {
                room: self.id.clone(),
                removed_tracks,
            })
            .await;
        }

        // Let everyone know we left
        self.send_room_event(RoomEvent::UserLeft {
            room_id: self.id.clone(),
            user_id: id.to_owned(),
        })
        .await;
    }

    /// Add a local track
    pub async fn add_track(
        &self,
        user_id: String,
        local_track: Arc<TrackLocalStaticRTP>,
    ) {
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

        self.publish(RoomEvent::RemoveTrack {
            room: self.id.clone(),
            removed_tracks: vec![id],
        })
        .await;
    }

    pub async fn publish_track(
        &self,
        router: Arc<LocalRouter>,
        receiver: Arc<dyn crate::track::receiver::Receiver>,
    ) {
        for peer in self.peers.iter() {
            let subscriber = peer.subscriber().await;
            let peer_id = peer.id();
            // no subscriber or same id = no publish
            if router.id() == peer_id || subscriber.is_none() {
                info!("Same id ({peer_id}) skipped.");
                continue;
            }

            info!("Publishing track to peer subsriber, peer_id: {}", peer_id);
            if router
                .add_down_tracks(peer.subscriber().await.unwrap(), Some(receiver.clone()))
                .await
                .is_err()
            {
                continue;
            }
        }
    }

    /// Close local track
    fn close_track(&self, id: &str) {
        info!("Track {id} has been removed");
        self.tracks.remove(id);

        // TODO: stop the RTP sender thread and drop
    }

    async fn send_room_event(&self, event: RoomEvent) {
        if let Ok(payload) = serde_json::to_string(&event) {
            info!("[Room {}] Sending room event: {}", self.id, payload);
            for peer in self.peers.iter() {
                if let Some(subscriber) = peer.subscriber().await {
                    subscriber.send_message(&payload).await;
                } else {
                    warn!("[{}] No subscriber available", peer.id());
                }
            }
        } else {
            error!("Error parsing {:?}", event);
        };
    }
}

#[derive(Debug)]
pub struct RoomSignal {
    pub id: String,
    sender: Sender<RoomEvent>,
}

impl RoomSignal {
    pub fn new(id: Option<String>) -> Arc<Self> {
        let (sender, _dropped) = channel::<RoomEvent>(10);
        Arc::new(Self {
            id: id.unwrap_or(Ulid::new().to_string()),
            sender,
        })
    }

    pub fn listener(&self) -> Receiver<RoomEvent> {
        self.sender.subscribe()
    }

    pub fn publish(&self, id: &str, event: RoomEvent) -> bool {
        info!("Room manager emitted {:?} for room {}", event, id);
        self.sender.clone().try_send(event).is_ok()
    }
}
