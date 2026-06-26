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
///
/// History:
/// - v5: keyboard-protocol negotiation, colored underlines
/// - v6: scripting messages: `RunCommand`, `SendInput`, `CapturePane` /
///   `CommandResult`, `PaneCapture`
/// - v7: `CaptureLastCommand`: block-scoped scripting read via OSC 133 marks
/// - v8: `ExecCommand` / `ExecDone`: synchronous `run` over OSC 133 completion
/// - v9: `CaptureLastBlock` / `BlockCapture`: structured last-block capture
///   (output text + exit code + command line)
/// - v10: `ClientHello.graphics`: per-client inline-graphics capabilities
///   (Kitty/Sixel/iTerm2)
pub const PROTOCOL_VERSION: u16 = 10;

/// Inline-graphics protocols the client's *outer* terminal supports, probed at
/// attach. The daemon renders images for a client only in a protocol its
/// terminal accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct GraphicsCaps {
    pub kitty: bool,
    pub sixel: bool,
    pub iterm2: bool,
}

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
    /// Inline-graphics protocols the client's outer terminal supports.
    pub graphics: GraphicsCaps,
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
    /// Execute `text` synchronously in a session's input target pane (CLI
    /// `run`): the daemon injects `text` + `\r`, waits for the OSC 133
    /// completion mark, and replies `ExecDone`, or
    /// `CommandResult { ok: false }` on any refusal (no session, no blocks,
    /// busy pane, alt screen, child exit, mid-command reset).
    ///
    /// **Postcard-positional**: always appended at the end of the enum.
    ExecCommand { session: Option<String>, text: String, timeout_ms: Option<u64> },
    /// Capture the last completed OSC 133 command block as structured parts
    /// (CLI `capture --last-command --json`). Replies with `BlockCapture` on
    /// success or `CommandResult { ok: false }` when no completed block
    /// exists.
    ///
    /// **Postcard-positional**: always appended at the end of the enum.
    CaptureLastBlock { session: Option<String> },
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
    /// Outcome of `RunCommand` / `SendInput`: `ok` drives the CLI exit code;
    /// `message` is the confirmation or error text.
    CommandResult { ok: bool, message: Option<String> },
    /// `CapturePane` response.
    PaneCapture { text: String },
    /// `ExecCommand` outcome: the command's recorded exit code (`None` when the
    /// `133;D` mark carried no payload), the completed block's output text,
    /// and whether the daemon-side wait timed out. Timeout is **structural**
    /// (this flag), never inferred from message text, so the CLI maps it to
    /// exit 124.
    ///
    /// **Postcard-positional**: always appended at the end of the enum.
    ExecDone { exit: Option<i32>, output: String, timed_out: bool },
    /// `CaptureLastBlock` response: the block's output text (same region as
    /// `CaptureLastCommand`), the closing `133;D` exit code (`None` when the
    /// mark carried no payload), and the command line typed at the prompt
    /// (`None` when the shell never emitted `133;B`/`133;C`).
    ///
    /// **Postcard-positional**: always appended at the end of the enum.
    BlockCapture { text: String, exit: Option<i32>, command_line: Option<String> },
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
        let client = ClientHello {
            version: PROTOCOL_VERSION,
            term: "xterm-256color".into(),
            kbd: NegotiatedKbd::Legacy,
            graphics: GraphicsCaps::default(),
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
            graphics: GraphicsCaps { kitty: true, sixel: false, iterm2: false },
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

    #[test]
    fn capture_last_block_round_trips() {
        // New pair in v9; appended at the end of their enums (postcard is positional).
        let requests = [
            ClientMsg::CaptureLastBlock { session: Some("work".into()) },
            ClientMsg::CaptureLastBlock { session: None },
        ];
        for m in requests {
            let enc = postcard::to_allocvec(&m).unwrap();
            assert_eq!(postcard::from_bytes::<ClientMsg>(&enc).unwrap(), m);
        }
        // All field shapes: exit Some/None × command_line Some/None.
        let replies = [
            ServerMsg::BlockCapture {
                text: "out1\nout2".into(),
                exit: Some(0),
                command_line: Some("cargo test".into()),
            },
            ServerMsg::BlockCapture {
                text: "boom".into(),
                exit: Some(127),
                command_line: None,
            },
            ServerMsg::BlockCapture {
                text: String::new(),
                exit: None,
                command_line: Some("true".into()),
            },
            ServerMsg::BlockCapture { text: String::new(), exit: None, command_line: None },
        ];
        for m in replies {
            let enc = postcard::to_allocvec(&m).unwrap();
            assert_eq!(postcard::from_bytes::<ServerMsg>(&enc).unwrap(), m);
        }
        // No-blocks refusal reuses `CommandResult` (same asymmetry as the siblings).
        let refusal = ServerMsg::CommandResult {
            ok: false,
            message: Some(
                "no command blocks — shell integration not active? see docs/command-blocks.md"
                    .into(),
            ),
        };
        let enc = postcard::to_allocvec(&refusal).unwrap();
        assert_eq!(postcard::from_bytes::<ServerMsg>(&enc).unwrap(), refusal);
    }

    #[test]
    fn exec_messages_round_trip() {
        // New pair in v8; appended at the end of their enums (postcard is positional).
        let requests = [
            ClientMsg::ExecCommand {
                session: Some("work".into()),
                text: "cargo test".into(),
                timeout_ms: Some(600_000),
            },
            ClientMsg::ExecCommand {
                session: None,
                text: "git rev-parse HEAD".into(),
                timeout_ms: None,
            },
        ];
        for m in requests {
            let enc = postcard::to_allocvec(&m).unwrap();
            assert_eq!(postcard::from_bytes::<ClientMsg>(&enc).unwrap(), m);
        }
        let replies = [
            ServerMsg::ExecDone { exit: Some(0), output: "ok\n".into(), timed_out: false },
            ServerMsg::ExecDone { exit: Some(124), output: "partial".into(), timed_out: false },
            // D mark with no exit payload.
            ServerMsg::ExecDone { exit: None, output: "out".into(), timed_out: false },
            // Structural timeout: no exit, empty output.
            ServerMsg::ExecDone { exit: None, output: String::new(), timed_out: true },
        ];
        for m in replies {
            let enc = postcard::to_allocvec(&m).unwrap();
            assert_eq!(postcard::from_bytes::<ServerMsg>(&enc).unwrap(), m);
        }
    }
}
