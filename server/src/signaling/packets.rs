use thiserror::Error;
use tokio_tungstenite::tungstenite::Message;

use volcano_sfu::rtc::{peer::JoinConfig, room::RoomInfo};
use webrtc::{
    ice_transport::ice_candidate::RTCIceCandidateInit,
    peer_connection::sdp::session_description::RTCSessionDescription,
};

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

/// Either description or ICE candidate
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Negotiation {
    /// Session Description
    SDP {
        description: RTCSessionDescription,
        media_type_buffer: Option<Vec<MediaType>>,
    },
    /// ICE Candidate
    ICE { candidate: ICECandidate },
}

/// Packet sent from the client to the server
#[derive(Deserialize, Debug)]
#[serde(tag = "type")]
pub enum PacketC2S {
    /// Answer (from client subscriber)
    Answer { description: RTCSessionDescription },
    /// Offer (from negotiation)
    Offer {
        id: u32,
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
        room_ids: Vec<String>,
    },
    /// Tell the server to send tracks
    Continue {
        id: u32,
        /// IDs of tracks the client wants
        tracks: Vec<String>,
    },
    /// Peer offers a description to a room
    Join {
        id: u32,
        room_id: String,
        offer: RTCSessionDescription,
        #[serde(default)]
        cfg: JoinConfig,
    },
    /// Removes current user from current room
    Leave,
    /// Tell the server certain tracks are no longer available
    Remove {
        /// IDs of tracks the client is no longer producing
        removed_tracks: Vec<String>,
    },
    /// Register candidate in local peer subscriber or publisher
    Trickle {
        candidate: RTCIceCandidateInit,
        target: u8,
    },
}

/// Packet sent from the server to the client
#[derive(Serialize, Debug)]
#[serde(tag = "type")]
pub enum PacketS2C {
    /// Accept authentication
    Accept {
        id: u32,
        available_rooms: Vec<RoomInfo>,
    },
    /// Answer (for client publisher)
    Answer {
        id: u32,
        description: RTCSessionDescription,
    },
    /// Tell the client certain tracks are no longer available
    Remove {
        /// Room emitting event
        room_id: String,
        /// IDs of tracks that are no longer being produced
        removed_tracks: Vec<String>,
    },
    /// Offer (for client subscriber)
    Offer {
        description: RTCSessionDescription,
    },
    /// Raw Message
    Message(String),
    /// Relay peer request
    RelayRequest {
        room_id: String,
        payload: String,
    },
    RoomCreate {
        id: String,
        owner: Option<String>,
    },
    RoomDelete {
        id: String,
    },
    Trickle {
        candidate: RTCIceCandidateInit,
        target: u8,
    },
    /// User joined the room
    UserJoin {
        /// Room emitting event
        room_id: String,
        /// User ID
        user_id: String,
    },
    /// User left the room
    UserLeft {
        /// Room emitting event
        room_id: String,
        /// ID of leaving user
        user_id: String,
    },
    /// Disconnection error
    Error {
        error: String,
    },
    /// Custom server error
    ServerError {
        error: ServerError,
    },
}

/// An error occurred on the server
#[derive(Error, Debug, Serialize)]
pub enum ServerError {
    #[error("This room ID does not exist.")]
    RoomNotFound,
    #[error("This track ID does not exist.")]
    TrackNotFound,
    #[error("Something went wrong trying to authenticate you.")]
    FailedToAuthenticate,
    #[error("Already connected to a room!")]
    AlreadyConnected,
    #[error("Not authenticated in this session.")]
    NotAuthenticated,
    #[error("Not connected to any room!")]
    NotConnected,
    #[error("Media type already has an existing track!")]
    MediaTypeSatisfied,
    #[error("Peer connection failed!")]
    PeerConnectionFailed,
    #[error("Bad Request. Reason: {reason}")]
    BadRequest { reason: String },
    #[error("Request type is unknown.")]
    UnknownRequest,
    #[error("Received message is not a text.")]
    UnproccesableEntity,
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
    pub fn from(message: &Message) -> Result<Self, ServerError> {
        if let Message::Text(text) = message {
            match serde_json::from_str(text) {
                Ok(packet) => Ok(packet),
                Err(e) => {
                    error!("Tried to parse packet: {text}");
                    let reason = e.to_string();
                    error!("Error: {reason}");
                    Err(ServerError::BadRequest { reason })
                }
            }
        } else {
            Err(ServerError::UnproccesableEntity)
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
