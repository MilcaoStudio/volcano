use thiserror::Error;

#[derive(Error, Debug, PartialEq)]
pub enum Error {
    #[error("relay peer is not ready")]
    ErrRelayPeerNotReady,
    #[error("relay peer signal already called")]
    ErrRelayPeerSignalDone,
    #[error("relay peer data channel is not ready")]
    ErrRelaySignalDCNotReady,

    #[error("relay peer send data failed")]
    ErrRelaySendDataFailed,

    #[error("relay request timeout")]
    ErrRelayRequestTimeout,

    #[error("relay request empty response")]
    ErrRelayRequestEmptyRespose,
}