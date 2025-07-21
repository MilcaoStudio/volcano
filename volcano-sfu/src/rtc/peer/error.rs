use thiserror::Error;
use webrtc::Error as RTCError;
use crate::track::error::Error as TrackError;
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Error, Debug, PartialEq)]
#[non_exhaustive]
pub enum Error {
    /// Transport established is required but not established
    #[error("no rtc transport exists for this peer")]
    ErrNoTransportEstablished,
    /// Offer received in unstable state
    #[error("offered ignored")]
    ErrOfferIgnored,
    #[error("WebRTC error: {0}")]
    ErrRTC(RTCError),
    #[error("Track error: {0}")]
    ErrTrack(TrackError),
}

impl Error {
    pub fn equal(&self, err: &anyhow::Error) -> bool {
        err.downcast_ref::<Self>() == Some(self)
    }
}

impl From<TrackError> for Error {
    fn from(error: TrackError) -> Self {
        match error {
            TrackError::ErrWebRTC(e) => Error::ErrRTC(e),
            err => Error::ErrTrack(err),
        }
    }
}

impl From<RTCError> for Error {
    fn from(error: RTCError) -> Self {
        Error::ErrRTC(error)
    }
}