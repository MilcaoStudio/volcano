use thiserror::Error;
use tokio_tungstenite::tungstenite::Message;

use volcano_sfu::rtc::{peer::{PeerConfig, PeerRole}, room::RoomInfo};
use webrtc::{
    ice_transport::{ice_candidate::RTCIceCandidateInit, ice_server::RTCIceServer},
    peer_connection::sdp::session_description::RTCSessionDescription,
};

pub(super) const HEARTBEAT_INTERVAL: u16 = 30_000; 

/// Available types of media tracks
#[derive(Debug, Clone, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub enum MediaType {
    /// Audio stream
    Audio,
    /// Video stream
    Video,
    /// Screenshare audio stream
    ScreenAudio,
    /// Screenshare video stream
    ScreenVideo,
}

/// Browser compliant ICE candidate
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ICECandidate {
    pub candidate: String,
    #[serde(default)]
    pub sdp_mid: String,
    #[serde(default)]
    pub sdp_mline_index: u16,
    #[serde(default)]
    pub username_fragment: String,
}

/// Packet sent from the client to the server
#[derive(Deserialize, Debug)]
#[serde(tag = "type")]
pub enum PacketC2S {
    /// Answer (from client subscriber)
    Answer { description: RTCSessionDescription },
    /// Offer (from negotiation)
    Offer {
        description: RTCSessionDescription,
    },
    /// Authenticate
    Connect {
        id: u32,
        /// Authentication token
        token: String,
        /// Rooms available for client
        // TODO: use revolt database for checking
        #[serde(default = "Vec::new")]
        #[allow(dead_code)]
        room_ids: Vec<String>,
    },
    /// Peer offers a description to a room
    Join {
        room_id: String,
        offer: Option<RTCSessionDescription>,
        #[serde(default)]
        cfg: PeerConfig,
    },
    /// Removes current user from current room
    Leave,
    /// Client should send Ping with a timestamp in every heartbeat.
    Ping {
        data: u64,
    },
    /// Register candidate in local peer subscriber or publisher
    Trickle {
        candidate: RTCIceCandidateInit,
        target: PeerRole,
    },
}

/// Packet sent from the server to the client
#[derive(Serialize, Debug)]
#[serde(tag = "type")]
pub enum PacketS2C {
    /// Accept authentication
    Accept {
        id: u32,
        user_id: String,
        ice_servers: Vec<RTCIceServer>,
        available_rooms: Vec<RoomInfo>,
    },
    /// Answer (for client publisher)
    Answer {
        description: RTCSessionDescription,
    },
    Hello {
        heartbeat_interval: u16
    },
    RoomInfo {
        room: RoomInfo,
    },
    /// Offer (for client subscriber)
    Offer {
        description: RTCSessionDescription,
    },
    /// Response to [PacketC2S::Ping]
    Pong {
        data: u64,
    },
    Trickle {
        candidate: RTCIceCandidateInit,
        target: PeerRole,
    },
}

// Incorrect format, or state
#[derive(Debug, Error, Serialize)]
#[serde(tag = "error", content = "reason", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum BadRequestError {
    #[error("Already connected to a room!")]
    AlreadyConnected,
    #[error("Bad Request. Reason: {reason}")]
    BadFormat { reason: String },
    #[error("Forbidden access. User is not authenticated in this session.")]
    Forbidden,
    #[error("Received message is not a text.")]
    UnproccesableEntity,
}

#[derive(Debug, Error, Serialize)]
#[serde(tag = "error", content = "reason", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum LostConnectionError {
    #[error("Lost connection to peer")]
    PeerConnectionLost,
    #[error("Lost connection to publisher")]
    PublisherConnectionLost,
    #[error("Lost connection from subscriber")]
    SubscriberConnectionLost,
}

/// An error occurred on the server
#[derive(Error, Debug, Serialize)]
#[serde(tag = "error", content = "reason", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ServerError {
    #[error("This room ID does not exist.")]
    RoomNotFound,
    #[error("Time for authentication expired.")]
    AuthTimeout,
}

impl std::fmt::Display for MediaType {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            MediaType::Audio => write!(f, "Audio"),
            MediaType::Video => write!(f, "Video"),
            MediaType::ScreenAudio => write!(f, "ScreenAudio"),
            MediaType::ScreenVideo => write!(f, "ScreenVideo"),
        }
    }
}

impl PacketC2S {
    /// Create a packet from incoming Message
    pub fn from(message: &Message) -> Result<Self, BadRequestError> {
        if let Message::Text(text) = message {
            match serde_json::from_str(text) {
                Ok(packet) => Ok(packet),
                Err(e) => {
                    error!("Tried to parse packet: {text}");
                    let reason = e.to_string();
                    error!("Error: {reason}");
                    Err(BadRequestError::BadFormat { reason })
                }
            }
        } else {
            Err(BadRequestError::UnproccesableEntity)
        }
    }
}

impl From<RTCIceCandidateInit> for ICECandidate {
    fn from(candidate: RTCIceCandidateInit) -> Self {
        let RTCIceCandidateInit {
            candidate,
            sdp_mid,
            sdp_mline_index,
            username_fragment,
        } = candidate;

        Self {
            candidate,
            sdp_mid: sdp_mid.unwrap_or_default(),
            sdp_mline_index: sdp_mline_index.unwrap_or_default(),
            username_fragment: username_fragment.unwrap_or_default(),
        }
    }
}

impl From<ICECandidate> for RTCIceCandidateInit {
    fn from(candidate: ICECandidate) -> Self {
        let ICECandidate {
            candidate,
            sdp_mid,
            sdp_mline_index,
            username_fragment,
        } = candidate;

        Self {
            candidate,
            sdp_mid: Some(sdp_mid),
            sdp_mline_index: Some(sdp_mline_index),
            username_fragment: Some(username_fragment),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteMedia {
    pub stream_id: String,
    pub video: String,
    pub frame_rate: String,
    pub audio: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layers: Option<Vec<String>>,
}
