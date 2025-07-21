use std::pin::Pin;

use async_trait::async_trait;
use webrtc::{ice_transport::{ice_candidate::RTCIceCandidateInit}, peer_connection::{offer_answer_options::RTCOfferOptions, sdp::session_description::RTCSessionDescription, OnICEConnectionStateChangeHdlrFn}};

pub mod api;
pub mod error;
mod publisher;
mod pubsub;
mod subscriber;
mod central;

pub use publisher::Publisher;
pub use subscriber::Subscriber;
pub use pubsub::PubSubPeer;
pub use central::CentralPeer;

pub const API_CHANNEL_LABEL: &str = "System";

pub type OnICEConnectionStateChangeFn = OnICEConnectionStateChangeHdlrFn;

pub type OnOfferFn = Box<
    dyn (FnMut(RTCSessionDescription) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>)
        + Send
        + Sync,
>;

pub type OnPubSubICECandidateFn = Box<
    dyn (FnMut(RTCIceCandidateInit, u8) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>)
        + Send
        + Sync,
>;

pub type OnICECandidateFn = Box<
dyn (FnMut(RTCIceCandidateInit) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>)
    + Send
    + Sync,
>;

pub type OnNegotiateFn =
    Box<dyn (FnMut(Option<RTCOfferOptions>) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'static>>)
     + Send + Sync>;

/// Peer configuration for enabling/disabling the publisher and/or the subscriber
#[derive(Debug, Default, Deserialize)]
pub struct PeerConfig {
    pub no_publish: bool,
    pub no_subscribe: bool,
    pub no_auto_subscribe: bool,
}

use error::Result;
use std::sync::Arc;
use super::room::Room;
#[async_trait]
pub trait Peer: Send + Sync {
    /// Sets remote and local descriptions for this peer. If this fails, a new offer should be created from client side.
    /// 
    /// If there are candidates awaiting to be added, they should be added to the peer connection and cleaned up.
    async fn answer(&self, sdp: RTCSessionDescription) -> Result<RTCSessionDescription>;

    /// Cleans up the peer connection and closes it.
    async fn clean_up(&self);

    /// Joins a room.
    /// 
    /// A new peer connection should be created and this peer should be added to the room.
    async fn join(self: &Arc<Self>, room: Arc<Room>) -> Result<()>;

    /// Starts negotiation process.
    /// 
    /// This peer must create an offer, and send it to the client. 
    async fn negotiate(&self, offer_options: Option<RTCOfferOptions>) -> Result<()>;

    /// Sets a function to be called when current peer connection receives an ICE candidate.
    async fn set_on_ice_candidate(&self, f: OnICECandidateFn);

    /// Sets a function to be called when current peer connection's state changes.
    async fn set_on_ice_connection_state_change(&self, f: OnICEConnectionStateChangeFn);
}