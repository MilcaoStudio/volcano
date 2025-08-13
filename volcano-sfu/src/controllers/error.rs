use thiserror::Error;

use crate::peer;

pub type Result<T> = core::result::Result<T, PeerControllerError>;

#[derive(Error, Debug, PartialEq)]
#[non_exhaustive]
pub enum PeerControllerError {
    #[error("no producer exists for this peer")]
    ErrNoProducer,
    #[error("no consumer exists for this peer")]
    ErrNoConsumer,
    #[error("track receiver is closed")]
    ErrReceiverClosed,
    #[error("WebRTC error: {0}")]
    ErrWebRTC(#[from] webrtc::Error),
    #[error("Peer error: {0}")]
    ErrPeer(#[from] peer::Error),
}