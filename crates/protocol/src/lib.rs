use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const PROTOCOL_VERSION: u16 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ClientMessage {
    Hello(ClientHello),
    AudioEnd,
    Ping { nonce: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClientHello {
    pub protocol_version: u16,
    pub client_name: String,
    pub diarization: DiarizationPreference,
    pub audio: AudioFormat,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DiarizationPreference {
    Prefer,
    Require,
    Disable,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AudioFormat {
    pub codec: AudioCodec,
    pub container: AudioContainer,
    pub sample_rate_hz: u32,
    pub channels: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AudioCodec {
    Opus,
    WavPcm16,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AudioContainer {
    Ogg,
    Wav,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ServerMessage {
    SessionReady(SessionReady),
    TranscriptPartial(TranscriptEvent),
    TranscriptFinal(TranscriptEvent),
    Warning(WarningMessage),
    Error(ErrorMessage),
    Pong { nonce: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionReady {
    pub session_id: Uuid,
    pub chunk_seconds: u64,
    pub diarization: DiarizationStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DiarizationStatus {
    Enabled,
    Disabled,
    Unsupported,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TranscriptEvent {
    pub session_id: Uuid,
    pub sequence: u64,
    pub received_at: DateTime<Utc>,
    pub start_ms: Option<u64>,
    pub end_ms: Option<u64>,
    pub speaker: Option<String>,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WarningMessage {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ErrorMessage {
    pub code: String,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_hello_without_session_language() {
        let hello: ClientMessage = serde_json::from_str(
            r#"{"hello":{"protocol_version":1,"client_name":"old-client","diarization":"disable","audio":{"codec":"wav_pcm16","container":"wav","sample_rate_hz":16000,"channels":1}}}"#,
        )
        .unwrap();
        let ClientMessage::Hello(hello) = hello else {
            panic!("expected hello");
        };
        assert_eq!(hello.language, None);
    }
}
