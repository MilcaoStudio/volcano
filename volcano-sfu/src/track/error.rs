use thiserror::Error;

use webrtc::rtcp::Error as RTCPError;
use webrtc::Error as RTCError;
use std::fmt;
use std::io::Error as IOError;
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Error, Debug, PartialEq)]
pub enum Error {
    // ErrTransportExists join is called after a peerconnection is established
    #[error("rtc transport already exists for this connection")]
    ErrTransportExists,
    // ErrNoTransportEstablished cannot signal before join
    #[error("no rtc transport exists for this Peer")]
    ErrNoTransportEstablished,
    // ErrOfferIgnored if offer received in unstable state
    #[error("offered ignored")]
    ErrOfferIgnored,
    // ErrTurnNoneAuthKey
    #[error("cannot get auth key from user map")]
    ErrTurnNoneAuthKey,
    #[error("webrtc error")]
    ErrWebRTC(RTCError),
    #[error("rtcp error")]
    ErrRTCP(RTCPError),
    #[error("no subscriber for this peer")]
    ErrNoSubscriber,
    #[error("data channel doesn't exist")]
    ErrDataChannelNotExists,
    #[error("no receiver found")]
    ErrNoReceiverFound,
    #[error("channel send error")]
    ErrChannelSend,
    // #[error("webrtc error error")]
    // ErrWebRTCError(WebRTCErrorError),
}
impl Error {
    pub fn equal(&self, err: &anyhow::Error) -> bool {
        err.downcast_ref::<Self>().map_or(false, |e| e == self)
    }
}

impl From<RTCError> for Error {
    fn from(error: RTCError) -> Self {
        Error::ErrWebRTC(error)
    }
}

impl From<RTCPError> for Error {
    fn from(error: RTCPError) -> Self {
        Error::ErrRTCP(error)
    }
}

pub struct ConfigError {
    pub value: ConfigErrorValue,
}
#[derive(Error, Debug)]
pub enum ConfigErrorValue {
    #[error("io error")]
    IOError(IOError),
}

impl From<IOError> for ConfigError {
    fn from(error: IOError) -> Self {
        ConfigError {
            value: ConfigErrorValue::IOError(error),
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Display::fmt(&self.value, f)
    }
}