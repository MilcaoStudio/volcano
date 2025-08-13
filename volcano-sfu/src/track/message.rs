#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteMedia {
    #[serde(rename = "streamId")]
    pub stream_id: String,
    #[serde(rename = "video")]
    pub video: String,
    #[serde(rename = "frameRate")]
    pub frame_rate: String,
    #[serde(rename = "audio")]
    pub audio: bool,
    #[serde(rename = "layers", skip_serializing_if = "Option::is_none")]
    pub layers: Option<Vec<String>>,
}