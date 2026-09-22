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
    #[serde(rename = "context.announce")]
    ContextAnnounce { providers: Vec<ProviderDescriptor> },
    #[serde(rename = "tool.result")]
    ToolResult {
        call_id: u64,
        ok: bool,
        text: String,
    },
}

/// One client-side context provider announced via `context.announce`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderDescriptor {
    pub id: String,
    pub tools: Vec<ToolDescriptor>,
}

/// A tool exposed by a client provider (bare name; the server namespaces it to
/// `personal.<provider>.<tool>`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDescriptor {
    pub name: String,
    pub description: String,
    /// JSON schema for the arguments object; defaults to an empty object.
    #[serde(default)]
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    State {
        state: SessionState,
    },
    #[serde(rename = "state.thinking")]
    StateThinking {
        /// `calling_tools` while a plugin/tool call is in flight; default = plain thinking.
        #[serde(default)]
        detail: ThinkingDetail,
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
    #[serde(rename = "tool.call")]
    ToolCall {
        call_id: u64,
        name: String,
        /// JSON-encoded arguments string, OpenAI style.
        arguments: String,
    },
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

/// Sub-detail for `state.thinking`: what "thinking" is currently doing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingDetail {
    #[default]
    Thinking,
    CallingTools,
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
    fn server_msg_state_thinking_wire_shape() {
        let msg = ServerMsg::StateThinking {
            detail: ThinkingDetail::CallingTools,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(
            json,
            r#"{"type":"state.thinking","detail":"calling_tools"}"#
        );
        let back: ServerMsg = serde_json::from_str(&json).unwrap();
        assert_eq!(back, msg);
        let default: ServerMsg = serde_json::from_str(r#"{"type":"state.thinking"}"#).unwrap();
        assert_eq!(
            default,
            ServerMsg::StateThinking {
                detail: ThinkingDetail::Thinking
            }
        );
    }

    #[test]
    fn context_announce_wire_shape() {
        // Single-line wire JSON: serde emits compact output, so the expected
        // string must be compact too (and balanced — the plan snippet omitted
        // the tool object's closing brace).
        let json = r#"{"type":"context.announce","providers":[{"id":"calendar","tools":[{"name":"calendar.events","description":"List events.","parameters":{"properties":{},"type":"object"}}]}]}"#;
        let de: ClientMsg = serde_json::from_str(json).unwrap();
        let ClientMsg::ContextAnnounce { providers } = de.clone() else {
            panic!("wrong variant")
        };
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].id, "calendar");
        assert_eq!(providers[0].tools[0].name, "calendar.events");
        assert_eq!(serde_json::to_string(&de).unwrap(), json);
    }

    #[test]
    fn tool_result_wire_shape() {
        let json = r#"{"type":"tool.result","call_id":7,"ok":true,"text":"3 events"}"#;
        let de: ClientMsg = serde_json::from_str(json).unwrap();
        assert_eq!(
            de,
            ClientMsg::ToolResult {
                call_id: 7,
                ok: true,
                text: "3 events".into()
            }
        );
        assert_eq!(serde_json::to_string(&de).unwrap(), json);
    }

    #[test]
    fn server_tool_call_wire_shape() {
        let msg = ServerMsg::ToolCall {
            call_id: 3,
            name: "personal.contacts.search".into(),
            arguments: r#"{"query":"mom"}"#.into(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(
            json,
            r#"{"type":"tool.call","call_id":3,"name":"personal.contacts.search","arguments":"{\"query\":\"mom\"}"}"#
        );
        let back: ServerMsg = serde_json::from_str(&json).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn unknown_type_rejected() {
        assert!(serde_json::from_str::<ClientMsg>(r#"{"type":"bogus"}"#).is_err());
        assert!(serde_json::from_str::<ServerMsg>(r#"{"type":"bogus"}"#).is_err());
        assert!(serde_json::from_str::<ClientMsg>(r#"{"pcm":"AAA="}"#).is_err());
    }
}
