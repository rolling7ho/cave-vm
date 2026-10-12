//! The `hello` handshake that opens every connection.
//!
//! The client lists the versions it speaks, and the server answers with the
//! highest one both speak, or an `unsupported_version` error.

use serde::{Deserialize, Serialize};

use crate::error::{ErrorCode, ErrorMessage};

/// `{"type": "hello", ...}`. The client's has `versions`, the server's has
/// `version`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub versions: Option<Vec<u32>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    /// The sender's name and version, such as `cave 0.1.0`. For logs only.
    pub software: String,
}

impl Hello {
    /// The client's hello, offering `versions`.
    pub fn client(versions: &[u32], software: impl Into<String>) -> Hello {
        Hello {
            versions: Some(versions.to_vec()),
            version: None,
            software: software.into(),
        }
    }

    /// The server's side: picks the highest version in the client's hello
    /// that is also in `ours`, and returns the reply to send.
    pub fn accept(&self, ours: &[u32], software: impl Into<String>) -> Result<Hello, ErrorMessage> {
        let Some(offered) = &self.versions else {
            return Err(ErrorMessage::new(
                ErrorCode::BadRequest,
                "the client's hello has no \"versions\"",
            ));
        };
        let Some(&version) = offered.iter().filter(|v| ours.contains(v)).max() else {
            return Err(ErrorMessage {
                code: ErrorCode::UnsupportedVersion,
                message: format!(
                    "no protocol version in common: offered {offered:?}, this side speaks {ours:?}"
                ),
                fix: None,
                supported: Some(ours.to_vec()),
            });
        };
        Ok(Hello {
            versions: None,
            version: Some(version),
            software: software.into(),
        })
    }

    /// The client's side: checks the server's reply picked one of the
    /// versions it offered, and returns it.
    pub fn agreed(&self, offered: &[u32]) -> Result<u32, ErrorMessage> {
        match self.version {
            Some(v) if offered.contains(&v) => Ok(v),
            Some(v) => Err(ErrorMessage::new(
                ErrorCode::UnsupportedVersion,
                format!("the server picked version {v}, which was not offered ({offered:?})"),
            )),
            None => Err(ErrorMessage::new(
                ErrorCode::BadRequest,
                "the server's hello has no \"version\"",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_picks_the_highest_common_version() {
        let hello = Hello::client(&[1, 2, 3], "cave 0.2.0");
        let reply = hello.accept(&[1, 2], "cave-agent 0.1.0").unwrap();
        assert_eq!(reply.version, Some(2));
        assert_eq!(reply.versions, None);
        assert_eq!(reply.agreed(&[1, 2, 3]), Ok(2));
    }

    #[test]
    fn no_common_version_lists_what_the_server_speaks() {
        let hello = Hello::client(&[1], "cave 0.1.0");
        let err = hello.accept(&[2, 3], "cave-agent 0.3.0").unwrap_err();
        assert_eq!(err.code, ErrorCode::UnsupportedVersion);
        assert_eq!(err.supported, Some(vec![2, 3]));
    }

    #[test]
    fn hellos_without_their_version_field_are_refused() {
        let server_style = Hello {
            versions: None,
            version: Some(1),
            software: "x".into(),
        };
        assert_eq!(
            server_style.accept(&[1], "y").unwrap_err().code,
            ErrorCode::BadRequest
        );
        let client_style = Hello::client(&[1], "x");
        assert_eq!(
            client_style.agreed(&[1]).unwrap_err().code,
            ErrorCode::BadRequest
        );
        let unoffered = Hello {
            versions: None,
            version: Some(9),
            software: "x".into(),
        };
        assert_eq!(
            unoffered.agreed(&[1]).unwrap_err().code,
            ErrorCode::UnsupportedVersion
        );
    }
}
