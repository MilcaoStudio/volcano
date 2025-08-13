use std::pin::Pin;

use async_trait::async_trait;
use webrtc::{
    data_channel::RTCDataChannel,
    ice_transport::ice_candidate::RTCIceCandidateInit,
    peer_connection::{
        OnICEConnectionStateChangeHdlrFn, offer_answer_options::RTCOfferOptions,
        sdp::session_description::RTCSessionDescription,
    },
};

#[cfg(test)]
pub(crate) mod peer_test;

pub mod api;
mod consumer;
mod error;
mod publisher;
mod subscriber;

pub use error::Error;
pub use error::Result;
pub use publisher::Publisher;
pub use subscriber::Subscriber;
pub use consumer::Consumer;

pub const API_CHANNEL_LABEL: &str = "System";

pub type OnICEConnectionStateChangeFn = OnICEConnectionStateChangeHdlrFn;

use std::sync::Arc;

#[async_trait]
pub trait Peer: Send + Sync {
    /// Adds a candidate to this peer.
    /// If the connection does not have a remote description yet, candidates must be stored until [Self::answer] is called.
    async fn add_ice_candidate(&self, candidate: RTCIceCandidateInit) -> Result<()>;

    /// Sets remote and local descriptions for this peer. If this fails, a new offer should be created from client side.
    ///
    /// If there are candidates awaiting to be added, they should be added to the peer connection and cleaned up.
    async fn answer(&self, sdp: RTCSessionDescription) -> Result<RTCSessionDescription>;

    /// Cleans up the peer connection and closes it.
    async fn clean_up(&self);

    /// Creates a [RTCDataChannel] from a label.
    /// The caller of this method should call [Self::negotitate] once to prevent pending negotiations.
    async fn create_data_channel(&self, label: String) -> Result<Arc<RTCDataChannel>>;

    /// Gets a [RTCDataChannel] by label, or None.
    async fn get_data_channel(&self, label: &str) -> Option<Arc<RTCDataChannel>>;

    /// Peer Id
    fn id(&self) -> String;

    /// Starts negotiation process.
    ///
    /// This peer must create an offer, and send it to the client.
    async fn negotiate(&self, offer_options: Option<RTCOfferOptions>) -> Result<()>;

    /// Sets a function to be called when current peer connection receives an ICE candidate.
    async fn set_on_ice_candidate(&self, f: OnICECandidateFn);

    /// Sets a function to be called when current peer connection's state changes.
    async fn set_on_ice_connection_state_change(&self, f: OnICEConnectionStateChangeFn);
}

pub type OnOfferFn = Box<
    dyn (Fn(RTCSessionDescription) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>)
        + Send
        + Sync,
>;

pub type OnICECandidateFn = Box<
    dyn (Fn(RTCIceCandidateInit) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>)
        + Send
        + Sync,
>;

pub type OnNegotiateFn = Box<
    dyn (Fn(Option<RTCOfferOptions>) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'static>>)
        + Send
        + Sync,
>;
