//! Wire protocol types for `WS /v1/realtime`.
//!
//! Client → server and server → client messages, JSON text frames, one message
//! per frame. Tags use dotted wire names exactly as the plan's protocol spec
//! (`session.start`, `audio.data`, `response.text.delta`, ...).

use serde::{Deserialize, Serialize};

fn default_rate() -> u32 {
    16_000
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    #[serde(rename = "session.start")]
    SessionStart {
        device_id: Option<String>,
        #[serde(default = "default_rate")]
        sample_rate: u32,
    },
    #[serde(rename = "audio.data")]
    AudioData {
        /// base64 of an arbitrary-length PCM16 chunk
        pcm: String,
    },
    #[serde(rename = "speech.end")]
    SpeechEnd,
    #[serde(rename = "session.stop")]
    SessionStop,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    State {
        state: SessionState,
    },
    Transcript {
        text: String,
    },
    #[serde(rename = "response.text.delta")]
    ResponseTextDelta {
        text: String,
    },
    #[serde(rename = "response.text")]
    ResponseText {
        text: String,
    },
    #[serde(rename = "audio.chunk")]
    AudioChunk {
        /// base64 PCM16 mono
        pcm: String,
        seq: u32,
    },
    #[serde(rename = "turn.completed")]
    TurnCompleted,
    Error {
        code: String,
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Listening,
    Speech,
    Thinking,
    Speaking,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_msg_roundtrip_all_variants() {
        let cases: Vec<(ClientMsg, &str)> = vec![
            (
                ClientMsg::SessionStart {
                    device_id: Some("esp32-kitchen".into()),
                    sample_rate: 16000,
                },
                r#"{"type":"session.start","device_id":"esp32-kitchen","sample_rate":16000}"#,
            ),
            (
                ClientMsg::AudioData { pcm: "AAA=".into() },
                r#"{"type":"audio.data","pcm":"AAA="}"#,
            ),
            (ClientMsg::SpeechEnd, r#"{"type":"speech.end"}"#),
            (ClientMsg::SessionStop, r#"{"type":"session.stop"}"#),
        ];
        for (msg, json) in cases {
            let ser = serde_json::to_string(&msg).unwrap();
            let de: ClientMsg = serde_json::from_str(&ser).unwrap();
            assert_eq!(de, msg, "roundtrip of {ser}");
            let de2: ClientMsg = serde_json::from_str(json).unwrap();
            assert_eq!(de2, msg, "wire json {json}");
        }
    }

    #[test]
    fn client_msg_session_start_defaults() {
        let de: ClientMsg = serde_json::from_str(r#"{"type":"session.start"}"#).unwrap();
        match de {
            ClientMsg::SessionStart {
                device_id,
                sample_rate,
            } => {
                assert_eq!(device_id, None);
                assert_eq!(sample_rate, 16000);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn server_msg_roundtrip_all_variants() {
        let cases: Vec<(ServerMsg, &str)> = vec![
            (
                ServerMsg::State {
                    state: SessionState::Listening,
                },
                r#"{"type":"state","state":"listening"}"#,
            ),
            (
                ServerMsg::Transcript {
                    text: "hello".into(),
                },
                r#"{"type":"transcript","text":"hello"}"#,
            ),
            (
                ServerMsg::ResponseTextDelta { text: "he".into() },
                r#"{"type":"response.text.delta","text":"he"}"#,
            ),
            (
                ServerMsg::ResponseText {
                    text: "hello there".into(),
                },
                r#"{"type":"response.text","text":"hello there"}"#,
            ),
            (
                ServerMsg::AudioChunk {
                    pcm: "AQID".into(),
                    seq: 3,
                },
                r#"{"type":"audio.chunk","pcm":"AQID","seq":3}"#,
            ),
            (ServerMsg::TurnCompleted, r#"{"type":"turn.completed"}"#),
            (
                ServerMsg::Error {
                    code: "upstream".into(),
                    message: "llm unreachable".into(),
                },
                r#"{"type":"error","code":"upstream","message":"llm unreachable"}"#,
            ),
        ];
        for (msg, json) in cases {
            let ser = serde_json::to_string(&msg).unwrap();
            let de: ServerMsg = serde_json::from_str(&ser).unwrap();
            assert_eq!(de, msg, "roundtrip of {ser}");
            let de2: ServerMsg = serde_json::from_str(json).unwrap();
            assert_eq!(de2, msg, "wire json {json}");
        }
    }

    #[test]
    fn session_state_serde() {
        for (st, tag) in [
            (SessionState::Listening, "listening"),
            (SessionState::Speech, "speech"),
            (SessionState::Thinking, "thinking"),
            (SessionState::Speaking, "speaking"),
        ] {
            let json = serde_json::to_string(&st).unwrap();
            assert_eq!(json, format!(r#""{tag}""#));
            let back: SessionState = serde_json::from_str(&json).unwrap();
            assert_eq!(back, st);
        }
    }

    #[test]
    fn unknown_type_rejected() {
        assert!(serde_json::from_str::<ClientMsg>(r#"{"type":"bogus"}"#).is_err());
        assert!(serde_json::from_str::<ServerMsg>(r#"{"type":"bogus"}"#).is_err());
        assert!(serde_json::from_str::<ClientMsg>(r#"{"pcm":"AAA="}"#).is_err());
    }
}
