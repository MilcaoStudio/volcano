use thiserror::Error;
use webrtc::rtcp::Error as RTCPError;
pub type Result<T> = std::result::Result<T, BufferError>;

#[derive(Error, Debug, PartialEq)]
pub enum BufferError {
    #[error("packet not found in cache")]
    ErrPacketNotFound,
    #[error("buffer too small")]
    ErrBufferTooSmall,
    #[error("received packet too old")]
    ErrPacketTooOld,
    #[error("packet already received")]
    ErrRTXPacket,
    #[error("packet is not large enough")]
    ErrShortPacket,
    #[error("invalid nil packet")]
    ErrNilPacket,
    #[error("io EOF")]
    ErrIOEof,
    #[error("rtcp error")]
    ErrRTCP(RTCPError),
}

impl BufferError {
    pub fn equal(&self, err: &anyhow::Error) -> bool {
        err.downcast_ref::<Self>().map_or(false, |e| e == self)
    }
}

impl From<RTCPError> for BufferError {
    fn from(error: RTCPError) -> Self {
        BufferError::ErrRTCP(error)
    }
}