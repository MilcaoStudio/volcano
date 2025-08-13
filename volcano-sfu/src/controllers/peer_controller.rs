use std::sync::Arc;

use async_trait::async_trait;
use webrtc::{data_channel::RTCDataChannel, peer_connection::{offer_answer_options::RTCOfferOptions, sdp::session_description::RTCSessionDescription, OnDataChannelHdlrFn}};

use crate::{peer::{Consumer, OnICEConnectionStateChangeFn}, track::router::LocalRouter};

use super::Result;

#[async_trait]
pub trait PeerController: Send + Sync + 'static {

    /// Peer consumer receives remote tracks and creates local tracks.
    async fn consumer(&self) -> Result<Arc<dyn Consumer + Send + Sync>>;

    /// Cleans up the peer connection and closes it.
    async fn close(&self);

    /// Closes the data channel related to the `label`, if exists.
    /// The channel should be deleted too.
    async fn close_local_data_channel(&self, label: &str) -> Result<()>;
    
    /// Creates a [RTCDataChannel] from a label, and stores it.
    /// This data channel is local and it shall be opened by a remote peer.
    /// Therefore, the caller of this method should call [Self::negotitate] and wait for a remote descripcion update.
    async fn create_local_data_channel(&self, label: String) -> Result<Arc<RTCDataChannel>>;
    
    /// Peer Id
    fn id(&self) -> String;

    /// The [RTCDataChannel] corresponding to `label`.
    async fn local_data_channel(&self, label: &str) -> Option<Arc<RTCDataChannel>>;
    
    /// Starts negotiation process.
    ///
    /// This peer must create an offer, and send it to the client.
    async fn negotiate(&self, offer_options: Option<RTCOfferOptions>) -> Result<()>;

    /// Sets a function to be called when a remote data channel is opened.
    async fn on_data_channel(&self, f: OnDataChannelHdlrFn);
    
    /// Sets remote description for this peer. If this fails, a new offer should be created from client side.
    ///
    /// If there are candidates awaiting to be added, they should be added to the peer connection and cleaned up.
    async fn on_remote_answer(&self, sdp: RTCSessionDescription) -> Result<()>;

    /// Sets remote and local descriptions for this peer. If this fails, a new offer should be created from client side.
    ///
    /// If there are candidates awaiting to be added, they should be added to the peer connection and cleaned up.
    async fn on_remote_offer(&self, sdp: RTCSessionDescription) -> Result<RTCSessionDescription>;

    /// Sets a function to be called when current peer connection's state changes.
    async fn on_ice_connection_state_change(&self, f: OnICEConnectionStateChangeFn);

    /// Peer router used for track forwarding to another peer
    async fn router(&self) -> Result<Arc<LocalRouter>>;
}