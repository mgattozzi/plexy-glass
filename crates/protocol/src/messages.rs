use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::time::SystemTime;

/// Window dimensions, in cells and pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PtySize {
    pub rows: u16,
    pub cols: u16,
    pub pixel_width: u16,
    pub pixel_height: u16,
}

/// A live session visible to `ListSessions`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionEntry {
    pub name: String,
    pub windows: u8,
    pub panes: u8,
    pub clients: u8,
    pub created: SystemTime,
}

/// What the daemon should spawn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnSpec {
    /// Argv\[0\] (the program). Resolved via `$PATH` by the daemon.
    pub program: String,
    /// Remaining argv.
    pub args: Vec<String>,
    /// Override environment (replaces the inherited env entirely).
    pub env: Vec<(String, String)>,
    /// Initial working directory; `None` means inherit the daemon's cwd.
    pub cwd: Option<String>,
}

/// Child exit outcome, mirrored from the daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExitStatus {
    /// Exited normally with the given code.
    Code(i32),
    /// Killed by signal `n`.
    Signal(i32),
    /// Status could not be determined.
    Unknown,
}

/// Bumped any time `ClientMsg` or `ServerMsg` changes meaning.
pub const PROTOCOL_VERSION: u16 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientHello {
    pub version: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerHello {
    pub version: u16,
    pub daemon_pid: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ClientMsg {
    AttachOrCreate {
        name: Option<String>,
        create_if_missing: bool,
        cmd: Option<SpawnSpec>,
        size: PtySize,
    },
    ListSessions,
    KillSession { name: String },
    Input(Bytes),
    Resize(PtySize),
    Detach,
    Shutdown,
    ReloadConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ServerMsg {
    Attached { session_name: String, client_id: u64 },
    SessionList { entries: Vec<SessionEntry> },
    SessionKilled { name: String },
    Output(Bytes),
    Exited { status: ExitStatus },
    Error(crate::errors::ProtocolError),
    ConfigReloaded { error: Option<String> },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pty_size_round_trips_through_postcard() {
        let size = PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 };
        let bytes = postcard::to_allocvec(&size).expect("serialize");
        let decoded: PtySize = postcard::from_bytes(&bytes).expect("deserialize");
        assert_eq!(size, decoded);
    }

    #[test]
    fn spawn_spec_round_trips_through_postcard() {
        let spec = SpawnSpec {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), "echo hi".into()],
            env: vec![("PATH".into(), "/usr/bin".into())],
            cwd: Some("/tmp".into()),
        };
        let bytes = postcard::to_allocvec(&spec).expect("serialize");
        let decoded: SpawnSpec = postcard::from_bytes(&bytes).expect("deserialize");
        assert_eq!(spec, decoded);
    }

    #[test]
    fn exit_status_variants_round_trip() {
        for status in [ExitStatus::Code(0), ExitStatus::Code(137), ExitStatus::Signal(9), ExitStatus::Unknown] {
            let bytes = postcard::to_allocvec(&status).expect("serialize");
            let decoded: ExitStatus = postcard::from_bytes(&bytes).expect("deserialize");
            assert_eq!(status, decoded);
        }
    }

    #[test]
    fn bytes_payload_round_trips() {
        let payload = Bytes::from_static(b"hello world");
        let bytes = postcard::to_allocvec(&payload).expect("serialize");
        let decoded: Bytes = postcard::from_bytes(&bytes).expect("deserialize");
        assert_eq!(payload, decoded);
    }

    #[test]
    fn client_msgs_round_trip() {
        let cases = vec![
            ClientMsg::AttachOrCreate {
                name: Some("main".into()),
                create_if_missing: true,
                cmd: Some(SpawnSpec {
                    program: "bash".into(),
                    args: vec![],
                    env: vec![],
                    cwd: None,
                }),
                size: PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
            },
            ClientMsg::ListSessions,
            ClientMsg::KillSession { name: "old".into() },
            ClientMsg::Input(Bytes::from_static(b"ls\n")),
            ClientMsg::Resize(PtySize { rows: 50, cols: 200, pixel_width: 0, pixel_height: 0 }),
            ClientMsg::Detach,
            ClientMsg::Shutdown,
        ];
        for msg in cases {
            let bytes = postcard::to_allocvec(&msg).expect("serialize");
            let decoded: ClientMsg = postcard::from_bytes(&bytes).expect("deserialize");
            assert_eq!(msg, decoded);
        }
    }

    #[test]
    fn server_msgs_round_trip_without_error_variant() {
        // Error variant is covered once `ProtocolError` lands.
        let cases = vec![
            ServerMsg::Attached { session_name: "main".into(), client_id: 0 },
            ServerMsg::SessionList { entries: vec![] },
            ServerMsg::SessionKilled { name: "old".into() },
            ServerMsg::Output(Bytes::from_static(b"hello")),
            ServerMsg::Exited { status: ExitStatus::Code(0) },
        ];
        for msg in cases {
            let bytes = postcard::to_allocvec(&msg).expect("serialize");
            let decoded: ServerMsg = postcard::from_bytes(&bytes).expect("deserialize");
            assert_eq!(msg, decoded);
        }
    }

    #[test]
    fn hello_round_trips() {
        let client = ClientHello { version: PROTOCOL_VERSION };
        let server = ServerHello { version: PROTOCOL_VERSION, daemon_pid: 12345 };

        let cb = postcard::to_allocvec(&client).expect("serialize");
        let sb = postcard::to_allocvec(&server).expect("serialize");

        assert_eq!(postcard::from_bytes::<ClientHello>(&cb).unwrap(), client);
        assert_eq!(postcard::from_bytes::<ServerHello>(&sb).unwrap(), server);
    }

    #[test]
    fn server_msg_error_round_trips() {
        let err = ServerMsg::Error(crate::errors::ProtocolError::VersionMismatch {
            client: 1,
            server: 2,
        });
        let bytes = postcard::to_allocvec(&err).expect("serialize");
        let decoded: ServerMsg = postcard::from_bytes(&bytes).expect("deserialize");
        assert_eq!(err, decoded);
    }
}
