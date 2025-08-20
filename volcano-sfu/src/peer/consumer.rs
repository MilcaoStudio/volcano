use std::sync::Arc;

use async_trait::async_trait;
use webrtc::rtp_transceiver::rtp_codec::{RTCRtpCodecCapability};

use crate::track::{downtrack::DownTrack, receiver::WebRTCReceiver};
use super::Result;

#[async_trait]
pub trait Consumer {

    /// Id used for logging
    fn id(&self) -> String;
    
    async fn new_local_track(&self, codec_capability: RTCRtpCodecCapability, receiver: &Arc<WebRTCReceiver>) -> Result<Arc<DownTrack>>;
    fn add_down_track(&self, down_track: Arc<DownTrack>);
    fn down_track_by_id(&self, id: &str) -> Option<Arc<DownTrack>>;
    
    async fn unsubscribe_track(&self, track_id: &str) -> Result<()>;
}