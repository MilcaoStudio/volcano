mod central;
mod error;
mod peer_controller;
pub mod pubsub;

pub use central::CentralController;
pub use error::Result;
pub use error::PeerControllerError;
pub use peer_controller::PeerController;

/// Peer configuration for enabling/disabling the publisher and/or the subscriber
#[derive(Debug, Default, Deserialize)]
pub struct PeerConfig {
    pub no_publish: bool,
    pub no_subscribe: bool,
    pub no_auto_subscribe: bool,
}