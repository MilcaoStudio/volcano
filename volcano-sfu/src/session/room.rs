use std::{
    collections::HashMap, fmt::Debug, ops::Deref, sync::{
        atomic::{AtomicBool, Ordering}, Arc
    }
};

use dashmap::{DashMap, Entry};

use tokio::sync::{broadcast::{self, Receiver, Sender}};
use webrtc::{
    track::track_local::{TrackLocal, track_local_static_rtp::TrackLocalStaticRTP},
};

use serde::Serialize;

use crate::{controllers::pubsub::PubSubController, track::{receiver::WebRTCReceiver, router::LocalRouter}};
use crate::controllers::PeerController;

/// Wrapper around `Arc<dyn PeerController>` to work around `DashMap`
/// lifetime inference issues with `dyn Trait` in concurrent contexts.
///
/// This allows storing peer controllers in `DashMap` without lifetime errors,
/// while preserving natural method access via `Deref` to `Arc<dyn PeerController>`.
#[derive(Clone)]
pub struct ArcPeerController(Arc<dyn PeerController>);

impl Deref for ArcPeerController {
    type Target = Arc<dyn PeerController>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<Arc<dyn PeerController>> for ArcPeerController {
    fn from(value: Arc<dyn PeerController>) -> Self {
        Self(value)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct UserStream {
    pub id: String,
    pub tracks: Vec<String>,
    pub simulcast: bool,
}

/// Room event which displays information happening in a WebRTC session.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", content = "data")]
#[non_exhaustive]
pub enum RoomEvent {
    /// This room is closed. *Not implemented*.
    RoomClosed(String),
    /// A peer joined this room. [UserStream]s should be received by client before the local subscriber starts sending tasks.
    RoomInfo(RoomInfo),
    /// Some [DownTrack]s were removed. The client must know about these tracks. 
    TracksRemoved {
        removed_tracks: Vec<String>,
        room_id: String,
    },
    /// [LocalRouter] observes voice activity from published tracks
    VoiceActivity {
        room_id: String,
        stream_ids: Vec<String>,
    },
    /// This room observes voice activity from published tracks.
    /// *Not implemented*.
    UserSpeaking {
        room_id: String,
        uid: String,
        sids: Vec<String>,
    },
    /// A new user joined this room. This event is triggered since a second peer joins.
    UserJoined {
        room_id: String,
        uid: String,
    },
    /// Local [Publisher] receives a remote track from an user.
    TrackAdded {
        room_id: String,
        uid: String,
        track: String,
        stream: UserStream,
    },
    /// User leaves this room. This event is not triggered when this room becomes empty.
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
pub struct Room {
    #[allow(dead_code)]
    pub id: String,
    //data_channels: Arc<Vec<Arc<DataChannel>>>,
    //labels: DashSet<String>,
    /// The room is already closed
    closed: Arc<AtomicBool>,
    event_sender: Sender<RoomEvent>,
    //participants: DashSet<String>,
    //audio_observer: Arc<Mutex<AudioObserver>>,
    //user_tracks: DashMap<String, Vec<String>>,
    pub user_streams: DashMap<String, Vec<UserStream>>,
    pub peers: Arc<DashMap<String, ArcPeerController>>,
    tracks: DashMap<String, Arc<TrackLocalStaticRTP>>,
}

impl Room {
    /// Create a new Room and initialise internal channels and maps
    pub fn new(id: String) -> Arc<Self> {
        let (s, _) = broadcast::channel::<RoomEvent>(8);
        
        Arc::new(Room {
            closed: Default::default(),
            //data_channels: Default::default(),
            id,
            event_sender: s,
            //labels: Default::default(),
            peers: Default::default(),
            //audio_observer: Arc::new(Mutex::new(audio_observer)),
            //user_tracks: Default::default(),
            user_streams: Default::default(),
            tracks: Default::default(),
        })
    }

    pub fn add_peer(&self, peer: Arc<dyn PeerController>) {
        let id = peer.id();
        self.peers.insert(id, peer.into());
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
    }

    pub fn get_peer(&self, peer_id: &str) -> Option<Arc<dyn PeerController>> {
        self.peers.get(peer_id).map(|peer| (**peer).clone())
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    pub fn is_empty(&self) -> bool {
        self.user_streams.len() == 0
    }

    /// Listen for events from the room
    pub fn subscribe_to_events(&self) -> Receiver<RoomEvent> {
        debug!("[Room {}] Added subscription to events", self.id);
        self.event_sender.subscribe()
    }

    pub async fn remove_peer(&self, peer_id: &str) -> usize {
        if let Some((_, peer)) = self.peers.remove(peer_id) {
            peer.close().await;
            debug!("peer: {} strong references left", Arc::strong_count(&peer));
        } else {
            debug!("Peer {peer_id} not found.");
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

    // PubSubController is currently the unique use case calling this function.
    pub async fn subscribe_peer(&self, peer: Arc<PubSubController>) {
        // Removed massive data channel creation
        
        if let Ok(router) = peer.router().await {
            router.start_audio_observer_task().await;
        }

        for cur_peer in self.peers.iter() {
            let cur_id = cur_peer.id();
            let peer_id = peer.id();
            if cur_id == peer_id {
                continue;
            }

            if let Ok(current_router) = cur_peer.router().await {
                info!(
                    "[Room {}] Peer {} subscribes to tracks from router {}. No receivers.",
                    self.id, peer_id, cur_id
                );

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

        self.trigger_event(RoomEvent::TracksRemoved {
            removed_tracks: vec![id],
            room_id: self.id.clone(),
        });
    }

    pub async fn publish_track(
        &self,
        router: &Arc<LocalRouter>,
        receiver: Arc<WebRTCReceiver>,
    ) {
        for peer in self.peers.iter() {
            let peer_id = peer.id();
            // no subscriber or same id = no publish
            if router.id() == peer_id {
                info!("Same id ({peer_id}) skipped.");
                continue;
            }

            let Some(sub) = peer.consumer().await else {
                warn!(
                    "[Room {}] Expected peer {} consumer. Got none.",
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
                        if let Err(err) = peer.negotiate(None).await {
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

}