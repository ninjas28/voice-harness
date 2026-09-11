//! Typed harness errors shared by core and providers.

use std::fmt;

#[derive(Debug, thiserror::Error)]
pub enum HarnessError {
    /// An upstream service (stt/tts/llm) returned a non-success response.
    #[error("{service} upstream error (HTTP {status}): {body}")]
    Upstream {
        service: String,
        status: u16,
        body: String,
    },
    /// Configuration problem (missing key, bad value).
    #[error("config error: {0}")]
    Config(String),
    /// Protocol-level problem (undecodable frame, bad envelope).
    #[error("protocol error: {0}")]
    Protocol(String),
    /// Local I/O failure.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

impl HarnessError {
    pub fn upstream(service: &str, status: u16, body: impl Into<String>) -> Self {
        HarnessError::Upstream {
            service: service.to_string(),
            status,
            body: body.into(),
        }
    }

    pub fn config(msg: impl Into<String>) -> Self {
        HarnessError::Config(msg.into())
    }

    pub fn protocol(msg: impl Into<String>) -> Self {
        HarnessError::Protocol(msg.into())
    }
}

impl fmt::Display for crate::types::SessionState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // keep wire spelling in logs too
        write!(f, "{}", serde_plain(self))
    }
}

fn serde_plain(state: &crate::types::SessionState) -> &'static str {
    match state {
        crate::types::SessionState::Listening => "listening",
        crate::types::SessionState::Speech => "speech",
        crate::types::SessionState::Thinking => "thinking",
        crate::types::SessionState::Speaking => "speaking",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as _;

    #[test]
    fn error_display_and_source() {
        let e = HarnessError::Upstream {
            service: "stt".to_string(),
            status: 502,
            body: "bad gateway".to_string(),
        };
        assert_eq!(e.to_string(), "stt upstream error (HTTP 502): bad gateway");
        assert!(HarnessError::Config("missing key".to_string())
            .to_string()
            .contains("config error: missing key"));

        let io_err = HarnessError::Io(std::io::Error::new(std::io::ErrorKind::Other, "disk"));
        let src = std::error::Error::source(&io_err);
        assert!(src.is_some());
        assert!(HarnessError::Upstream {
            service: "tts".to_string(),
            status: 500,
            body: String::new(),
        }
        .source()
        .is_none());
    }

    #[test]
    fn constructors_and_state_display() {
        assert_eq!(
            HarnessError::upstream("llm", 401, "unauthorized").to_string(),
            "llm upstream error (HTTP 401): unauthorized"
        );
        assert_eq!(crate::types::SessionState::Thinking.to_string(), "thinking");
    }
}
