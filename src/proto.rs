//! Wire protocol: newline-delimited JSON, identical over a Unix socket (local)
//! and over an SSH pipe (remote). PTY bytes ride as base64.

use anyhow::Result;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "t")]
pub enum Req {
    List,
    Create {
        agent: String,
        name: String,
        cwd: String,
        cols: u16,
        rows: u16,
    },
    Attach {
        id: String,
        cols: u16,
        rows: u16,
    },
    Input {
        id: String,
        data: String,
    },
    Resize {
        id: String,
        cols: u16,
        rows: u16,
    },
    Kill {
        id: String,
    },
    Restart {
        id: String,
        cols: u16,
        rows: u16,
    },
}

/// Struct variants only: serde's internally-tagged repr cannot serialize a newtype
/// variant wrapping a sequence, and a silently-dropped frame is a miserable bug.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "t")]
pub enum Resp {
    Sessions { sessions: Vec<SessionInfo> },
    Output { id: String, data: String },
    Exited { id: String, code: i32 },
    Error { msg: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionInfo {
    pub id: String,
    pub agent: String,
    pub name: String,
    pub cwd: String,
    pub status: Status,
    pub created_at: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Status {
    Running,
    Stopped,
}

// ponytail: base64-in-JSON costs ~33% on PTY output. Fine at terminal data rates;
// swap Output for a length-prefixed binary frame only if a profiler says so.
pub fn b64(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

pub fn unb64(s: &str) -> Result<Vec<u8>> {
    Ok(STANDARD.decode(s)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let reqs = vec![
            Req::List,
            Req::Create {
                agent: "claude".into(),
                name: "api".into(),
                cwd: "/tmp".into(),
                cols: 80,
                rows: 24,
            },
            Req::Input {
                id: "x".into(),
                data: b64(b"hi\x1b[0m"),
            },
        ];
        for r in reqs {
            let line = serde_json::to_string(&r).unwrap();
            assert!(!line.contains('\n'), "frames must be single-line");
            assert_eq!(r, serde_json::from_str::<Req>(&line).unwrap());
        }

        // Every Resp variant must survive the wire — a variant serde silently
        // refuses to encode disappears at runtime instead of failing loudly.
        let resps = vec![
            Resp::Sessions {
                sessions: vec![SessionInfo {
                    id: "1".into(),
                    agent: "claude".into(),
                    name: "api".into(),
                    cwd: "/tmp".into(),
                    status: Status::Stopped,
                    created_at: 7,
                }],
            },
            Resp::Sessions { sessions: vec![] },
            Resp::Output {
                id: "x".into(),
                data: b64(&[0u8, 255, 10, 13]),
            },
            Resp::Exited {
                id: "x".into(),
                code: -1,
            },
            Resp::Error { msg: "boom".into() },
        ];
        for r in resps {
            let line = serde_json::to_string(&r).expect("every variant must serialize");
            assert!(!line.contains('\n'));
            assert_eq!(r, serde_json::from_str::<Resp>(&line).unwrap());
        }
    }

    #[test]
    fn base64_is_byte_exact() {
        let raw: Vec<u8> = (0..=255u8).collect();
        assert_eq!(unb64(&b64(&raw)).unwrap(), raw);
    }
}
