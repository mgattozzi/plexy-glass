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

/// A session saved on disk, reported by `ListSavedSessions`. May or may not
/// be currently running.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedSessionEntry {
    pub name: String,
    pub windows: u8,
    pub panes: u8,
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
///
/// History:
/// - v5: keyboard-protocol negotiation, colored underlines
/// - v6: scripting messages: `RunCommand`, `SendInput`, `CapturePane` /
///   `CommandResult`, `PaneCapture`
/// - v7: `CaptureLastCommand`: block-scoped scripting read via OSC 133 marks
pub const PROTOCOL_VERSION: u16 = 7;

/// Which keyboard protocol a client negotiated with its *outer* terminal. The
/// daemon decodes that client's input bytes in this protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NegotiatedKbd {
    Legacy,
    /// modifyOtherKeys level (1 or 2).
    ModifyOtherKeys(u8),
    /// Kitty keyboard-protocol flags the client pushed on attach.
    Kitty(u8),
}

/// Outer-terminal color preference relayed from the client's `\e[?997;Xn`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ColorScheme {
    Dark,
    Light,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientHello {
    pub version: u16,
    /// The client's `$TERM`, advertised to panes via XTGETTCAP `TN`.
    pub term: String,
    /// The keyboard protocol the client negotiated with its outer terminal.
    pub kbd: NegotiatedKbd,
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
    ListSavedSessions,
    KillSession { name: String },
    Input(Bytes),
    Resize(PtySize),
    Detach,
    Shutdown,
    ReloadConfig,
    /// Outer terminal gained focus (`\e[I`).
    FocusIn,
    /// Outer terminal lost focus (`\e[O`).
    FocusOut,
    /// Outer terminal reported its color scheme (`\e[?997;Xn`).
    ColorScheme(ColorScheme),
    /// Run one command-prompt line against a session (CLI `cmd`).
    RunCommand { session: Option<String>, line: String },
    /// Write raw bytes into a session's input path (CLI `send`).
    SendInput { session: Option<String>, bytes: Bytes },
    /// Capture the focused pane's visible screen text (CLI `capture`).
    CapturePane { session: Option<String> },
    /// Capture the last completed OSC 133 command block's output text
    /// (scrollback-inclusive). Replies with `PaneCapture` on success or
    /// `CommandResult { ok: false }` when no completed block exists.
    ///
    /// **Postcard-positional**: always appended at the end of the enum.
    CaptureLastCommand { session: Option<String> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ServerMsg {
    Attached { session_name: String, client_id: u64 },
    SessionList { entries: Vec<SessionEntry> },
    SavedSessionList { entries: Vec<SavedSessionEntry> },
    SessionKilled { name: String },
    Output(Bytes),
    Exited { status: ExitStatus },
    Error(crate::errors::ProtocolError),
    ConfigReloaded { error: Option<String> },
    /// Outcome of `RunCommand` / `SendInput`: `ok` drives the CLI exit code;
    /// `message` is the confirmation or error text.
    CommandResult { ok: bool, message: Option<String> },
    /// `CapturePane` response.
    PaneCapture { text: String },
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
            ClientMsg::ListSavedSessions,
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
            ServerMsg::SavedSessionList {
                entries: vec![SavedSessionEntry { name: "alpha".into(), windows: 2, panes: 3 }],
            },
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
        let client = ClientHello {
            version: PROTOCOL_VERSION,
            term: "xterm-256color".into(),
            kbd: NegotiatedKbd::Legacy,
        };
        let server = ServerHello { version: PROTOCOL_VERSION, daemon_pid: 12345 };

        let cb = postcard::to_allocvec(&client).expect("serialize");
        let sb = postcard::to_allocvec(&server).expect("serialize");

        assert_eq!(postcard::from_bytes::<ClientHello>(&cb).unwrap(), client);
        assert_eq!(postcard::from_bytes::<ServerHello>(&sb).unwrap(), server);
    }

    #[test]
    fn negotiated_kbd_round_trips() {
        for kbd in [
            NegotiatedKbd::Legacy,
            NegotiatedKbd::ModifyOtherKeys(2),
            NegotiatedKbd::Kitty(31),
        ] {
            let bytes = postcard::to_allocvec(&kbd).expect("serialize");
            let decoded: NegotiatedKbd = postcard::from_bytes(&bytes).expect("deserialize");
            assert_eq!(kbd, decoded);
        }
    }

    #[test]
    fn client_hello_with_caps_round_trips() {
        let hello = ClientHello {
            version: PROTOCOL_VERSION,
            term: "xterm-ghostty".into(),
            kbd: NegotiatedKbd::Kitty(31),
        };
        let bytes = postcard::to_allocvec(&hello).expect("serialize");
        let decoded: ClientHello = postcard::from_bytes(&bytes).expect("deserialize");
        assert_eq!(hello, decoded);
    }

    #[test]
    fn focus_and_color_scheme_msgs_round_trip() {
        let cases = vec![
            ClientMsg::FocusIn,
            ClientMsg::FocusOut,
            ClientMsg::ColorScheme(ColorScheme::Dark),
            ClientMsg::ColorScheme(ColorScheme::Light),
        ];
        for msg in cases {
            let bytes = postcard::to_allocvec(&msg).expect("serialize");
            let decoded: ClientMsg = postcard::from_bytes(&bytes).expect("deserialize");
            assert_eq!(msg, decoded);
        }
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

    #[test]
    fn scripting_messages_round_trip() {
        let msgs = [
            ClientMsg::RunCommand { session: Some("w".into()), line: "split v".into() },
            ClientMsg::RunCommand { session: None, line: "layout tiled".into() },
            ClientMsg::SendInput { session: None, bytes: Bytes::from_static(b"ls\r") },
            ClientMsg::CapturePane { session: Some("w".into()) },
        ];
        for m in msgs {
            let enc = postcard::to_allocvec(&m).unwrap();
            assert_eq!(postcard::from_bytes::<ClientMsg>(&enc).unwrap(), m);
        }
        let outs = [
            ServerMsg::CommandResult { ok: true, message: None },
            ServerMsg::CommandResult { ok: false, message: Some("nope".into()) },
            ServerMsg::PaneCapture { text: "hello\nworld".into() },
        ];
        for m in outs {
            let enc = postcard::to_allocvec(&m).unwrap();
            assert_eq!(postcard::from_bytes::<ServerMsg>(&enc).unwrap(), m);
        }
    }

    #[test]
    fn capture_last_command_round_trips() {
        // New variant in v7; appended at the end of the enum (postcard is positional).
        let cases = [
            ClientMsg::CaptureLastCommand { session: Some("work".into()) },
            ClientMsg::CaptureLastCommand { session: None },
        ];
        for m in cases {
            let enc = postcard::to_allocvec(&m).unwrap();
            assert_eq!(postcard::from_bytes::<ClientMsg>(&enc).unwrap(), m);
        }
        // Daemon replies reuse `PaneCapture` (success) and `CommandResult` (failure).
        for reply in [
            ServerMsg::PaneCapture { text: "out1\nout2".into() },
            ServerMsg::CommandResult {
                ok: false,
                message: Some(
                    "no command blocks — shell integration not active? see docs/command-blocks.md"
                        .into(),
                ),
            },
        ] {
            let enc = postcard::to_allocvec(&reply).unwrap();
            assert_eq!(postcard::from_bytes::<ServerMsg>(&enc).unwrap(), reply);
        }
    }
}
