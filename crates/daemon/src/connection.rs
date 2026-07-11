//! One connection from a client.

use std::cmp::Ordering as CmpOrdering;
use std::io::Error;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use std::{env, fs, future, mem};

use plexy_glass_mux::{
    Command, Keymap, KeymapAction, PaletteEntry, PromptCommand, VirtualScreen, block_mode, blocks,
    command_prompt, copy_mode, hint, palette,
};
use plexy_glass_protocol::errors::CodecError;
use plexy_glass_protocol::{
    ClientMsg, Codec, ProtocolError, PtySize, ServerMsg, SpawnSpec, server_handshake,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, watch};
use tokio::{io, task, time};

use crate::declared::expand_tilde;
use crate::error::DaemonError;
use crate::input_router::decode_protocol;
use crate::registry::SessionRegistry;
use crate::renderer::{RenderInject, Renderer};
use crate::session::coordinator::binding_keys;
use crate::session::{Session, SessionHistory, SessionTree};
use crate::window_manager::{OverlayKeyResult, Severity};
use crate::{InputEvent, InputRouter, osc_actions};

pub async fn serve<S>(
    stream: S,
    daemon_pid: u32,
    registry: Arc<SessionRegistry>,
    config: Arc<plexy_glass_config::Config>,
) -> Result<(), DaemonError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut reader, mut writer) = io::split(stream);
    let client_hello = server_handshake(&mut reader, &mut writer, daemon_pid).await?;

    let frame = Codec::read_frame(&mut reader)
        .await?
        .ok_or_else(|| DaemonError::Io(Error::other("client closed before first message")))?;
    let msg: ClientMsg =
        postcard::from_bytes(&frame).map_err(|e| CodecError::Decode(e.to_string()))?;

    match msg {
        ClientMsg::ListSessions => {
            let entries = registry.list().await;
            send_msg(&mut writer, &ServerMsg::SessionList { entries }).await?;
            Ok(())
        }
        ClientMsg::KillSession { name } => match registry.kill(&name).await {
            Ok(()) => send_msg(&mut writer, &ServerMsg::SessionKilled { name }).await,
            Err(DaemonError::Protocol(perr)) => {
                send_msg(&mut writer, &ServerMsg::Error(perr)).await
            }
            Err(e) => Err(e),
        },
        ClientMsg::AttachOrCreate {
            name,
            create_if_missing,
            cmd,
            size,
        } => {
            serve_attach(
                reader,
                writer,
                registry,
                name,
                create_if_missing,
                cmd,
                size,
                config,
                client_hello,
            )
            .await
        }
        ClientMsg::ReloadConfig => {
            let error = match registry.reload_config().await {
                Ok(()) => None,
                Err(e) => Some(e.to_string()),
            };
            send_msg(&mut writer, &ServerMsg::ConfigReloaded { error }).await?;
            Ok(())
        }
        ClientMsg::RunCommand { session, line } => {
            let (ok, message) = match resolve_session(&registry, session).await {
                Err(msg) => (false, Some(msg)),
                Ok(sess) => run_prompt_line(&sess, &registry, &line).await,
            };
            send_msg(&mut writer, &ServerMsg::CommandResult { ok, message }).await?;
            Ok(())
        }
        ClientMsg::SendInput { session, bytes } => {
            let (ok, message) = match resolve_session(&registry, session).await {
                Err(msg) => (false, Some(msg)),
                Ok(sess) => match sess.handle_input_bytes(&bytes, false).await {
                    Ok(()) => (true, None),
                    Err(e) => (false, Some(e.to_string())),
                },
            };
            send_msg(&mut writer, &ServerMsg::CommandResult { ok, message }).await?;
            Ok(())
        }
        ClientMsg::CapturePane { session } => {
            // Response-type asymmetry by design: success replies
            // `PaneCapture`, every error replies `CommandResult{ok:false}`,
            // and the CLI client matches on either.
            let reply = match resolve_session(&registry, session).await {
                Err(msg) => ServerMsg::CommandResult {
                    ok: false,
                    message: Some(msg),
                },
                Ok(sess) => {
                    let text = {
                        let manager = sess.window_manager.lock().await;
                        manager
                            .input_target_pane()
                            .map(|p| p.with_screen(plexy_glass_mux::screen_text))
                    };
                    match text {
                        Some(text) => ServerMsg::PaneCapture { text },
                        // Unreachable in practice: a session with no panes
                        // tears itself down.
                        None => ServerMsg::CommandResult {
                            ok: false,
                            message: Some("no focused pane".into()),
                        },
                    }
                }
            };
            send_msg(&mut writer, &reply).await?;
            Ok(())
        }
        ClientMsg::CaptureLastCommand { session } => {
            // Response-type asymmetry by design: success replies
            // `PaneCapture`, every error (no session, no pane, no completed
            // block) replies `CommandResult{ok:false}`, and the CLI client
            // matches on either.
            let reply = match resolve_session(&registry, session).await {
                Err(msg) => ServerMsg::CommandResult {
                    ok: false,
                    message: Some(msg),
                },
                Ok(sess) => {
                    let text = {
                        let manager = sess.window_manager.lock().await;
                        manager.input_target_pane().and_then(|p| {
                            p.with_screen(|s| {
                                plexy_glass_mux::last_completed_block(s)
                                    .map(|range| plexy_glass_mux::block_text(s, range))
                            })
                        })
                    };
                    match text {
                        Some(text) => ServerMsg::PaneCapture { text },
                        None => ServerMsg::CommandResult {
                            ok: false,
                            message: Some(NO_BLOCKS_MSG.into()),
                        },
                    }
                }
            };
            send_msg(&mut writer, &reply).await?;
            Ok(())
        }
        ClientMsg::ExecCommand {
            session,
            text,
            timeout_ms,
        } => {
            serve_exec(
                &mut reader,
                &mut writer,
                &registry,
                session,
                text,
                timeout_ms,
            )
            .await
        }
        ClientMsg::CaptureLastBlock { session } => {
            // Response-type asymmetry by design: success replies
            // `BlockCapture`, every error (no session, no pane, no completed
            // block) replies `CommandResult{ok:false}`, and the CLI client
            // matches on either.
            let reply = match resolve_session(&registry, session).await {
                Err(msg) => ServerMsg::CommandResult {
                    ok: false,
                    message: Some(msg),
                },
                Ok(sess) => {
                    let parts = {
                        let manager = sess.window_manager.lock().await;
                        manager.input_target_pane().and_then(|p| {
                            p.with_screen(|s| {
                                blocks::last_completed_prompt(s).map(|prompt| {
                                    // `block_output_range` only returns None when no
                                    // `PROMPT_START` exists at or above the line, and
                                    // `prompt` IS a `PROMPT_START` line, so the fallback is
                                    // unreachable. Kept defensive per the no-unwrap rule.
                                    let range = plexy_glass_mux::block_output_range(s, prompt)
                                        .unwrap_or((prompt, prompt));
                                    (
                                        plexy_glass_mux::block_text(s, range),
                                        blocks::closing_exit(s, prompt),
                                        blocks::block_command_line(s, prompt),
                                    )
                                })
                            })
                        })
                    };
                    match parts {
                        Some((text, exit, command_line)) => ServerMsg::BlockCapture {
                            text,
                            exit,
                            command_line,
                        },
                        None => ServerMsg::CommandResult {
                            ok: false,
                            message: Some(NO_BLOCKS_MSG.into()),
                        },
                    }
                }
            };
            send_msg(&mut writer, &reply).await?;
            Ok(())
        }
        other => {
            send_msg(
                &mut writer,
                &ServerMsg::Error(ProtocolError::UnexpectedMessage(format!("{other:?}"))),
            )
            .await?;
            Ok(())
        }
    }
}

async fn send_msg<W>(writer: &mut W, msg: &ServerMsg) -> Result<(), DaemonError>
where
    W: AsyncWrite + Unpin,
{
    let bytes = postcard::to_allocvec(msg).map_err(|e| CodecError::Encode(e.to_string()))?;
    Codec::write_frame(writer, &bytes).await?;
    Ok(())
}

#[allow(
    clippy::too_many_arguments,
    reason = "single internal entry point; refactoring loses clarity"
)]
async fn serve_attach<R, W>(
    mut reader: R,
    mut writer: W,
    registry: Arc<SessionRegistry>,
    name: Option<String>,
    create_if_missing: bool,
    cmd: Option<SpawnSpec>,
    mut size: PtySize,
    config: Arc<plexy_glass_config::Config>,
    client_hello: plexy_glass_protocol::ClientHello,
) -> Result<(), DaemonError>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    // Per-connection decode context from the handshake. `kbd` scopes THIS
    // client's key decode (deterministic, replacing the Permissive default).
    let client_kbd = client_hello.kbd;
    let client_remote = client_hello.remote;
    let client_version = client_hello.version;

    // Resolve or create the session. `session` is reassigned in place by
    // `switch_session` when the client switches to another session.
    let mut session = if let Some(n) = name {
        match registry.get(&n).await {
            Some(s) => s,
            None if create_if_missing => {
                let spec = cmd.unwrap_or_else(default_spawn_spec);
                let cfg = Arc::clone(&config);
                // `attach_or_create` restores from disk if a saved file exists.
                match registry.attach_or_create(n.clone(), spec, size, cfg).await {
                    Ok(s) => s,
                    Err(DaemonError::Protocol(perr)) => {
                        return send_msg(&mut writer, &ServerMsg::Error(perr)).await;
                    }
                    Err(e) => return Err(e),
                }
            }
            None => {
                return send_msg(
                    &mut writer,
                    &ServerMsg::Error(ProtocolError::SessionNotFound { name: n }),
                )
                .await;
            }
        }
    } else {
        // No name means the default session "main": attach-or-create,
        // deterministic regardless of what else is running. (The old
        // sole-session fallback silently attached to a config-declared
        // session when it was the only one.)
        let spec = cmd.unwrap_or_else(default_spawn_spec);
        let cfg = Arc::clone(&config);
        // `attach_or_create` restores "main" from disk if saved.
        match registry
            .attach_or_create("main".into(), spec, size, cfg)
            .await
        {
            Ok(s) => s,
            Err(DaemonError::Protocol(perr)) => {
                return send_msg(&mut writer, &ServerMsg::Error(perr)).await;
            }
            Err(e) => return Err(e),
        }
    };

    // This connection's live prefix-armed flag. The input loop stores the
    // keymap state into it after every consume; the session's render paths
    // read it (via the registered ClientHandle) for the any-client-armed
    // aggregate behind the `prefix-indicator` widget.
    let prefix_active = Arc::new(AtomicBool::new(false));

    // Register this connection as a client. `register_client` calls
    // `blocking_lock` internally, so dispatch it off the async runtime.
    let session_for_register = Arc::clone(&session);
    let prefix_for_register = Arc::clone(&prefix_active);
    let handle = match task::spawn_blocking(move || {
        session_for_register.register_client(size, prefix_for_register, client_remote)
    })
    .await
    {
        Ok(Ok(h)) => h,
        Ok(Err(DaemonError::Protocol(perr))) => {
            return send_msg(&mut writer, &ServerMsg::Error(perr)).await;
        }
        Ok(Err(e)) => return Err(e),
        Err(join) => return Err(DaemonError::Io(Error::other(join.to_string()))),
    };

    let mut client_id = handle.client_id;
    let session_name = session.name();

    send_msg(
        &mut writer,
        &ServerMsg::Attached {
            session_name,
            client_id,
        },
    )
    .await?;

    // Spawn the per-Connection renderer task. It owns the writer half from
    // here on out. `switch_tx` lets the input loop re-point the renderer at a
    // different session's frame stream (session switch) without reclaiming the
    // writer.
    let frame_rx = handle.frame_rx.clone();
    let (switch_tx, switch_rx) = mpsc::unbounded_channel::<watch::Receiver<Arc<VirtualScreen>>>();
    // `inject_tx` lets the input loop hand the renderer a message to WRITE (or
    // an invalidate) without reclaiming the writer it owns, e.g. a v12+
    // client's `OpenSessionPicker` on `Ctrl+a w`.
    let (inject_tx, inject_rx) = mpsc::unbounded_channel::<RenderInject>();
    let mut renderer = Renderer::new();
    // Thread this client's negotiated graphics caps so the renderer emits image
    // protocols only its outer terminal supports.
    let g = client_hello.graphics;
    renderer.set_graphics_caps(plexy_glass_mux::GraphicsCaps {
        kitty: g.kitty,
        sixel: g.sixel,
        iterm2: g.iterm2,
    });
    let mut renderer_task = tokio::spawn(async move {
        let _ = renderer.run(frame_rx, switch_rx, inject_rx, writer).await;
    });

    // Input loop. Scope key decode to the client's negotiated outer-terminal
    // protocol (older/unknown peers downgraded to Legacy upstream).
    let mut router = InputRouter::with_protocol(decode_protocol(client_kbd));
    let (km, keymap_skips) = plexy_glass_keys::build_keymap_with_skips(&config.keymap);
    let mut keymap = km;

    // Show the one-time welcome modal on the first attach to this daemon, gated
    // by the `welcome` config knob (the user's on/off switch; `welcome #false`
    // turns it off). `take_welcome` flips an in-memory daemon-lifetime flag, no
    // on-disk marker. A config error preempts it (the modal is deferred until a
    // clean config). The `&&` short-circuit means a disabled/preempted welcome
    // never consumes the slot. Tests set `PLEXY_GLASS_NO_WELCOME` to suppress it.
    let has_config_error = registry.has_config_error();
    let show_welcome = !has_config_error
        && config.welcome
        && env::var_os("PLEXY_GLASS_NO_WELCOME").is_none()
        && registry.take_welcome();
    if show_welcome {
        {
            let mut m = session.window_manager.lock().await;
            m.open_welcome();
        }
        session.notify.notify_one();
    } else if let Some((severity, text)) = attach_notice(has_config_error, keymap_skips.len()) {
        session.set_status_message(text, severity).await;
    }

    // Esc-disambiguation window: a lone `\x1b` (or a partial CSI) parks in the
    // input parsers awaiting more bytes. If nothing more arrives within this
    // window, flush it, so a lone ESC becomes `Key(Escape)` (the only way a bare
    // Esc cancels an overlay on legacy / modifyOtherKeys clients). Long enough
    // that a real `\x1b[…` split across reads still arrives as one sequence,
    // short enough to feel instant.
    const IDLE_FLUSH: Duration = Duration::from_millis(30);

    // Cancel-safety: `Codec::read_frame` is `read_exact`-based and NOT
    // cancel-safe (dropping it mid-frame loses buffered bytes), so it is
    // pinned and polled across iterations, recreated only after it
    // completes. The idle-flush timer is the cancel-safe arm; it is gated
    // by `armed` so when the parser is idle it is never polled (no
    // busy-wake), matching the `serve_exec` discipline.
    let mut read_fut = Box::pin(Codec::read_frame(&mut reader));
    let idle_flush = time::sleep(IDLE_FLUSH);
    tokio::pin!(idle_flush);
    // Whether the parser is mid-escape AND we have set the timer deadline for
    // this pending state. Reset to the `IDLE_FLUSH` deadline the first time a
    // frame leaves the parser pending; cleared once it drains or the timer
    // fires (so the next pending state re-arms freshly).
    let mut armed = false;

    loop {
        enum Wake {
            Frame(bytes::Bytes),
            IdleFlush,
            Stop,
        }
        let wake = tokio::select! {
            biased;
            // Renderer exits when its `frame_rx` is closed, i.e. the session's
            // coordinator dropped its `frame_tx`. That means the session ended
            // (last pane exited, or the session was killed). Tear down so the
            // client process exits and `HostTty::restore` runs.
            _ = &mut renderer_task => Wake::Stop,
            // The idle-flush fires only when armed (a lone ESC / partial CSI is
            // parked). When not armed the branch is disabled, never polled.
            () = &mut idle_flush, if armed => Wake::IdleFlush,
            result = &mut read_fut => match result {
                Ok(Some(f)) => Wake::Frame(f),
                Ok(None) | Err(_) => Wake::Stop,
            },
        };
        let frame = match wake {
            Wake::Stop => break,
            Wake::IdleFlush => {
                // The disambiguation window elapsed with no follow-on byte:
                // flush the parked sequence and dispatch the resulting event
                // (a lone ESC → `Key(Escape)`) through the SAME routing as any
                // key, so Esc-cancel reaches the open overlay.
                armed = false;
                if let Some(event) = router.flush_keys() {
                    let focus_before = session.active_pane_id().await;
                    let detach = {
                        let mut ctx = ClientCtx {
                            session: &mut session,
                            client_id: &mut client_id,
                            size,
                            registry: &registry,
                            switch_tx: &switch_tx,
                            prefix_armed: &prefix_active,
                            remote: client_remote,
                            version: client_version,
                            inject_tx: &inject_tx,
                        };
                        dispatch_input_event(&mut ctx, &mut keymap, client_kbd, event).await
                    };
                    if let (Some(before), Some(after)) =
                        (focus_before, session.active_pane_id().await)
                        && before != after
                    {
                        session.synthesize_focus_transition(before, after).await;
                    }
                    if detach {
                        break;
                    }
                }
                continue;
            }
            Wake::Frame(f) => {
                // Recreate the pinned read future for the next iteration, and only
                // ever after it has completed, so no buffered frame is lost.
                // Drop the old (completed) future FIRST to release its borrow of
                // `reader` before the new one reborrows it.
                read_fut = {
                    drop(read_fut);
                    Box::pin(Codec::read_frame(&mut reader))
                };
                f
            }
        };
        let msg: ClientMsg = match postcard::from_bytes(&frame) {
            Ok(m) => m,
            Err(_) => continue,
        };
        match msg {
            ClientMsg::Input(bytes) => {
                let events = router.classify(bytes.as_ref());
                let mut detach_requested = false;
                // Snapshot the focused pane before the whole batch; if any event
                // in it switched the active pane (select-pane, a click,
                // choose-tree, …), synthesize focus-out(old)/focus-in(new) for
                // ?1004 subscribers after the batch.
                let focus_before = session.active_pane_id().await;
                for event in events {
                    let mut ctx = ClientCtx {
                        session: &mut session,
                        client_id: &mut client_id,
                        size,
                        registry: &registry,
                        switch_tx: &switch_tx,
                        prefix_armed: &prefix_active,
                        remote: client_remote,
                        version: client_version,
                        inject_tx: &inject_tx,
                    };
                    if dispatch_input_event(&mut ctx, &mut keymap, client_kbd, event).await {
                        detach_requested = true;
                        break;
                    }
                }
                if let (Some(before), Some(after)) = (focus_before, session.active_pane_id().await)
                    && before != after
                {
                    session.synthesize_focus_transition(before, after).await;
                }
                if detach_requested {
                    break;
                }
                // Arm (or disarm) the Esc idle-flush based on whether this
                // batch left the parser mid-escape. Reset the deadline only on
                // the transition into pending so a stream of input frames can't
                // keep pushing it out. Between a lone ESC and the next byte
                // there are no frames, which is exactly when it must fire.
                if router.has_pending() {
                    if !armed {
                        idle_flush.as_mut().reset(time::Instant::now() + IDLE_FLUSH);
                        armed = true;
                    }
                } else {
                    armed = false;
                }
            }
            ClientMsg::Resize(new_size) => {
                // Track the client's size so a later session switch can register
                // on the target session at the correct dimensions.
                size = new_size;
                let session_for_resize = Arc::clone(&session);
                let cid = client_id;
                let _ = task::spawn_blocking(move || {
                    session_for_resize.handle_resize(cid, new_size);
                })
                .await;
            }
            ClientMsg::Detach | ClientMsg::Shutdown => break,
            ClientMsg::FocusIn => {
                // Any-client-focused rule: emit focus-in only when the aggregate
                // transitions from no-client-focused to some-client-focused. The
                // per-pane ?1004 gate lives in `focus_active_pane`.
                if let Some(now) = session.set_client_focus(client_id, true).await {
                    session.focus_active_pane(now).await;
                }
            }
            ClientMsg::FocusOut => {
                // Emit focus-out only when ALL attached clients have lost focus.
                if let Some(now) = session.set_client_focus(client_id, false).await {
                    session.focus_active_pane(now).await;
                }
            }
            ClientMsg::ColorScheme(scheme) => {
                // Most-recently-active client's preference wins; forward to all
                // ?2031 subscribers.
                let dark = matches!(scheme, plexy_glass_protocol::ColorScheme::Dark);
                session.forward_color_scheme(dark).await;
            }
            ClientMsg::SwitchSession { name } => {
                // Same-daemon fast switch (the v12+ client picker's same-host
                // path): reuse the overlay-driven machinery. It already
                // invalidates the renderer via `switch_tx`, so no extra redraw.
                let mut ctx = ClientCtx {
                    session: &mut session,
                    client_id: &mut client_id,
                    size,
                    registry: &registry,
                    switch_tx: &switch_tx,
                    prefix_armed: &prefix_active,
                    remote: client_remote,
                    version: client_version,
                    inject_tx: &inject_tx,
                };
                // On success `switch_session` invalidates via `switch_tx`. A
                // failure (target vanished between picker-open and Enter)
                // repaints nothing, but the client already cleared its screen
                // for the picker — so force a repaint over the blank.
                if !ctx.switch_session(name).await {
                    let _ = inject_tx.send(RenderInject::Invalidate);
                }
            }
            ClientMsg::Redraw => {
                let _ = inject_tx.send(RenderInject::Invalidate);
            }
            _ => {}
        }
    }
    cleanup_and_exit(session, client_id, renderer_task).await
}

async fn cleanup_and_exit(
    session: Arc<Session>,
    client_id: u64,
    renderer_task: task::JoinHandle<()>,
) -> Result<(), DaemonError> {
    // A floating popup is transient: it does not survive detach (any client).
    {
        let mut m = session.window_manager.lock().await;
        m.close_popup();
    }
    let session_for_dereg = Arc::clone(&session);
    let _ = task::spawn_blocking(move || {
        session_for_dereg.deregister_client(client_id);
    })
    .await;
    renderer_task.abort();
    Ok(())
}

/// Dispatch one classified input event through the full per-key routing
/// (overlay → popup → keymap → copy-mode/shell), or route a paste/byte. Called
/// once per event in the batch loop AND by the Esc idle-flush (so a flushed
/// `Key(Escape)` takes the exact same overlay/keymap path as any other key).
/// Returns `true` iff the event requested a detach (the caller breaks the loop).
///
/// `Bytes` and `Paste` are DISCARDED while an overlay is open, since the modal
/// owns input and nothing should leak to the pane's child behind it. (Copy mode
/// is a separate modal surface, routed inside the `Key` arm below.)
async fn dispatch_input_event(
    ctx: &mut ClientCtx<'_>,
    keymap: &mut Keymap,
    client_kbd: plexy_glass_protocol::NegotiatedKbd,
    event: InputEvent,
) -> bool {
    match event {
        InputEvent::Mouse(me) => {
            let _ = ctx.session.handle_mouse(me).await;
            // Status-bar Detach click sets WindowManager.detach_requested.
            // Propagate it so this connection exits.
            let mut mgr = ctx.session.window_manager.lock().await;
            if mgr.detach_requested {
                mgr.detach_requested = false;
                return true;
            }
        }
        InputEvent::Key(ke, raw_bytes) => {
            // An open overlay (rename / help / picker) captures every key
            // before the keymap or the shell, the same routing as copy mode
            // below. The overlay was opened by a Command, so the opening
            // keystroke already went through the keymap; every subsequent key
            // lands here until commit/cancel.
            let overlay_active = {
                let m = ctx.session.window_manager.lock().await;
                m.overlay().is_some()
            };
            if overlay_active {
                let result = {
                    let mut m = ctx.session.window_manager.lock().await;
                    m.handle_overlay_key(&ke)
                };
                return apply_overlay_result(ctx, keymap, result).await;
            }
            // A floating popup is modal: keys still run through the keymap (so
            // the close/open chords fire), but any OTHER recognized command is
            // swallowed, and PassThrough bytes go to the POPUP's child instead
            // of the active layout pane. This must precede the copy-mode
            // routing below, since a pre-existing copy-mode pane must not
            // steal popup keys.
            let popup_open = ctx.session.popup_active().await;
            if popup_open {
                let action = keymap.consume(ke, raw_bytes);
                store_prefix_armed(ctx.prefix_armed, keymap, ctx.session);
                match action {
                    KeymapAction::PassThrough(event_ke, bytes_back) => {
                        let _ = ctx
                            .session
                            .handle_popup_key_event(&event_ke, &bytes_back, client_kbd)
                            .await;
                    }
                    KeymapAction::Command(
                        cmd @ (Command::ClosePopup | Command::OpenPopup { .. }),
                    ) => {
                        if let Err(e) = ctx.session.handle_command(cmd).await {
                            ctx.session.set_status_error(e.to_string()).await;
                        }
                    }
                    KeymapAction::Command(_) | KeymapAction::Pending | KeymapAction::Cancel => {
                        ctx.session.notify.notify_one();
                    }
                }
                return false;
            }
            let action = keymap.consume(ke, raw_bytes);
            store_prefix_armed(ctx.prefix_armed, keymap, ctx.session);
            // Snap scrollback to live on any keystroke, EXCEPT the
            // block-scroll verbs (they SET the offset; resetting first would
            // pin every press to the newest prompt) and a pending prefix chord
            // (resetting on the prefix key itself would break the second
            // `prefix <` the same way; the chord's final command decides).
            let keeps_scroll = matches!(
                action,
                KeymapAction::Pending
                    | KeymapAction::Command(
                        Command::PrevPrompt | Command::NextPrompt | Command::CopyOutput
                    )
            );
            if !keeps_scroll {
                let manager = ctx.session.window_manager.lock().await;
                if let Some(p) = manager.active_window().active_pane() {
                    p.reset_scroll();
                }
            }
            match action {
                KeymapAction::PassThrough(event_ke, bytes_back) => {
                    // If the active pane is in copy mode, route the key event
                    // to the CopyModeHandler instead of the shell.
                    let active_in_copy_mode = {
                        let m = ctx.session.window_manager.lock().await;
                        m.active_window()
                            .active_pane()
                            .is_some_and(super::pane::Pane::is_in_copy_mode)
                    };
                    let active_in_block_mode = {
                        let m = ctx.session.window_manager.lock().await;
                        m.active_window()
                            .active_pane()
                            .is_some_and(super::pane::Pane::is_in_block_mode)
                    };
                    if active_in_copy_mode {
                        let action = {
                            let m = ctx.session.window_manager.lock().await;
                            let pane_opt = m.active_window().active_pane();
                            pane_opt.and_then(|p| {
                                let screen = p.with_screen(Clone::clone);
                                p.with_copy_mode_mut(|state| {
                                    copy_mode::handle(&event_ke, state, &screen)
                                })
                            })
                        };
                        match action {
                            Some(plexy_glass_mux::CopyModeAction::Render) => {
                                ctx.session.notify.notify_one();
                            }
                            Some(plexy_glass_mux::CopyModeAction::Exit) => {
                                let m = ctx.session.window_manager.lock().await;
                                if let Some(p) = m.active_window().active_pane() {
                                    p.exit_copy_mode();
                                }
                                ctx.session.notify.notify_one();
                            }
                            Some(plexy_glass_mux::CopyModeAction::Yank(text)) => {
                                let wrote = osc_actions::write_clipboard(text.as_bytes()).await;
                                // Honest message: a failed clipboard write must not
                                // claim "✓ copied". The text is still in the paste
                                // buffer, so the warn points at Ctrl+a ].
                                let (msg, sev) = osc_actions::yank_status(wrote, &text, true);
                                // Also push a paste buffer (before re-taking the
                                // WM lock, so the registry await isn't held under it).
                                ctx.registry.push_paste_buffer(text.into_bytes()).await;
                                {
                                    let m = ctx.session.window_manager.lock().await;
                                    if let Some(p) = m.active_window().active_pane() {
                                        p.exit_copy_mode();
                                    }
                                }
                                // `set_status_message` notifies + schedules the TTL wake.
                                ctx.session.set_status_message(msg, sev).await;
                            }
                            None => {}
                        }
                    } else if active_in_block_mode {
                        let action = {
                            let m = ctx.session.window_manager.lock().await;
                            let pane_opt = m.active_window().active_pane();
                            pane_opt.and_then(|p| {
                                let screen = p.with_screen(Clone::clone);
                                p.with_block_mode_mut(|state| {
                                    block_mode::handle(&event_ke, state, &screen)
                                })
                            })
                        };
                        match action {
                            Some(plexy_glass_mux::BlockModeAction::Render) => {
                                ctx.session.notify.notify_one();
                            }
                            Some(plexy_glass_mux::BlockModeAction::Exit) => {
                                let m = ctx.session.window_manager.lock().await;
                                if let Some(p) = m.active_window().active_pane() {
                                    p.exit_block_mode();
                                }
                                ctx.session.notify.notify_one();
                            }
                            Some(plexy_glass_mux::BlockModeAction::Yank(text)) => {
                                let wrote = osc_actions::write_clipboard(text.as_bytes()).await;
                                // Honest message (see copy-mode yank above); the
                                // text is in the paste buffer, so warn points there.
                                let (msg, sev) = osc_actions::yank_status(wrote, &text, true);
                                // STAY in block mode (unlike copy mode's yank).
                                ctx.registry.push_paste_buffer(text.into_bytes()).await;
                                ctx.session.set_status_message(msg, sev).await;
                            }
                            Some(plexy_glass_mux::BlockModeAction::ReRun(cmd)) => {
                                // Inject command + Enter directly into the pane
                                // (bypassing sync-panes, like serve_exec), then
                                // exit and snap to live to watch it run.
                                let pane = {
                                    let m = ctx.session.window_manager.lock().await;
                                    m.active_window().active_pane().cloned()
                                };
                                if let Some(p) = pane {
                                    let mut bytes = cmd.into_bytes();
                                    bytes.push(b'\r');
                                    let _ = p.send_input(bytes::Bytes::from(bytes)).await;
                                    p.exit_block_mode();
                                    p.reset_scroll();
                                }
                                ctx.session.notify.notify_one();
                            }
                            Some(plexy_glass_mux::BlockModeAction::ToggleFold(line)) => {
                                let m = ctx.session.window_manager.lock().await;
                                if let Some(p) = m.active_window().active_pane() {
                                    p.with_screen_mut(|s| {
                                        blocks::toggle_block_fold(s, line);
                                    });
                                }
                                ctx.session.notify.notify_one();
                            }
                            Some(plexy_glass_mux::BlockModeAction::FoldAll) => {
                                let m = ctx.session.window_manager.lock().await;
                                if let Some(p) = m.active_window().active_pane() {
                                    p.with_screen_mut(blocks::fold_all_completed);
                                }
                                ctx.session.notify.notify_one();
                            }
                            Some(plexy_glass_mux::BlockModeAction::UnfoldAll) => {
                                let m = ctx.session.window_manager.lock().await;
                                if let Some(p) = m.active_window().active_pane() {
                                    p.with_screen_mut(blocks::unfold_all);
                                }
                                ctx.session.notify.notify_one();
                            }
                            Some(plexy_glass_mux::BlockModeAction::Ignore) | None => {}
                        }
                    } else {
                        let _ = ctx
                            .session
                            .handle_key_event(&event_ke, &bytes_back, client_kbd)
                            .await;
                    }
                }
                KeymapAction::Command(cmd) => match ConnVerb::from_command(cmd) {
                    Ok(verb) => {
                        if run_connection_verb(ctx, keymap, verb).await {
                            return true;
                        }
                    }
                    Err(Command::CommandPrompt) => {
                        // Opened here (not in handle_command) because it needs
                        // the live session list for `switch ` Tab-completion.
                        let names: Vec<String> = ctx
                            .registry
                            .list()
                            .await
                            .into_iter()
                            .map(|e| e.name)
                            .collect();
                        {
                            let mut m = ctx.session.window_manager.lock().await;
                            m.open_command_prompt(names);
                        }
                        ctx.session.notify.notify_one();
                    }
                    Err(other) => {
                        if let Err(e) = ctx.session.handle_command(other).await {
                            ctx.session.set_status_error(e.to_string()).await;
                        }
                    }
                },
                KeymapAction::Pending | KeymapAction::Cancel => {
                    ctx.session.notify.notify_one();
                }
            }
        }
        // An open overlay is modal: a paste must not leak to the pane's child
        // behind it (an Esc-then-paste bail, or a real paste while a picker is
        // up). Discard it; the modal owns input.
        InputEvent::Paste(bytes) => {
            let overlay_active = {
                let m = ctx.session.window_manager.lock().await;
                m.overlay().is_some()
            };
            if !overlay_active {
                paste_bytes(ctx.session, bytes).await;
            }
        }
        // Likewise for raw passthrough bytes (e.g. an Esc-then-non-printable
        // bail the parser maps to bytes): swallowed while an overlay is open.
        InputEvent::Bytes(bs) => {
            let overlay_active = {
                let m = ctx.session.window_manager.lock().await;
                m.overlay().is_some()
            };
            if !overlay_active {
                let _ = ctx.session.handle_input_bytes(&bs, false).await;
            }
        }
    }
    false
}

/// Publish the keymap's prefix-armed state after a `Keymap::consume`, and
/// repaint iff it TRANSITIONED. The flag is read by the session's render
/// paths (any-client-armed aggregate → `prefix-indicator` widget), but
/// storing it does not wake the render loop by itself, so:
///
/// - disarmed→armed (`Pending`): notify here. (The `Pending` arm also
///   notifies, and `Notify` coalesces permits into one wakeup.)
/// - armed→disarmed via `Command`/`Cancel`: notify here; redundant with the
///   repaints those paths already trigger, again coalesced. (These are the
///   ONLY disarm paths: an armed prefix waits indefinitely, no timeout, so
///   `PassThrough` can't occur mid-chord.)
/// - no transition (plain typing, `PassThrough` with prefix idle): no
///   notify, so ordinary keystrokes don't force a status repaint.
fn store_prefix_armed(flag: &Arc<AtomicBool>, keymap: &Keymap, session: &Arc<Session>) {
    let armed = keymap.prefix_active();
    if flag.swap(armed, Ordering::SeqCst) != armed {
        session.notify.notify_one();
    }
}

/// Per-client connection state threaded through session switches and
/// cross-session actions (overlay results, connection-layer verbs). Bundles
/// what was a 5-argument tuple. `session`/`client_id` are `&mut` because a
/// switch re-points both in place. The input loop constructs the bundle at
/// each dispatch site instead of holding one long-lived instance, since the
/// batch body also uses `session` directly between dispatches, so a loop-long
/// `&mut` borrow would not check.
struct ClientCtx<'a> {
    session: &'a mut Arc<Session>,
    client_id: &'a mut u64,
    size: PtySize,
    registry: &'a Arc<SessionRegistry>,
    switch_tx: &'a mpsc::UnboundedSender<watch::Receiver<Arc<VirtualScreen>>>,
    /// The connection's live prefix-armed flag; re-registered on the target
    /// session during a switch so re-arming keeps working afterwards.
    prefix_armed: &'a Arc<AtomicBool>,
    /// Whether the connection reached the daemon over `-H`/SSH; re-registered on
    /// the target session during a switch so the `ssh` marker survives the switch.
    remote: bool,
    /// The client's negotiated protocol version; gates v12+ features (the
    /// client-rendered picker) so an older downgraded client keeps the daemon
    /// overlay.
    version: u16,
    /// Out-of-band sender to this connection's renderer task.
    inject_tx: &'a mpsc::UnboundedSender<RenderInject>,
}

impl ClientCtx<'_> {
    /// Re-point this client at another running session in place. Registers on
    /// the target *before* deregistering the source so the client is never
    /// momentarily unattached, hands the renderer the target's frame stream
    /// (forcing a full repaint), then swaps the loop's `session`/`client_id`.
    /// All failure paths land on the transient status line and leave the
    /// client on the source session.
    ///
    /// Returns `true` iff the client actually moved to a different session.
    /// Callers that follow a switch with target-scoped work (e.g. focusing a
    /// window/pane by id) MUST gate that work on `true`, because on failure
    /// `session` still points at the source, and because pane/window ids are
    /// not unique across sessions, applying the target's ids to the source
    /// would silently mutate the wrong session.
    async fn switch_session(&mut self, name: String) -> bool {
        let target = if let Some(t) = self.registry.get(&name).await {
            t
        } else {
            // Not live: auto-create it if it's a declared template. The config
            // comes from the live session's own per-session snapshot (the
            // ClientCtx has no config field), and the build uses this client's
            // real size. Unknown-AND-undeclared names keep the existing error.
            let config = self.session.config_snapshot();
            if let Some(template) = config.sessions.iter().find(|t| t.name == name) {
                match self
                    .registry
                    .create_declared(template, Arc::clone(&config), self.size)
                    .await
                {
                    Ok(t) => t,
                    Err(e) => {
                        self.session
                            .set_status_error(format!("cannot switch to {name}: {e}"))
                            .await;
                        return false;
                    }
                }
            } else {
                self.session
                    .set_status_error(format!("no session: {name}"))
                    .await;
                return false;
            }
        };
        if target.name() == self.session.name() {
            self.session
                .set_status_info(format!("already on {name}"))
                .await;
            return false;
        }
        // `register_client` takes a `blocking_lock` internally, so keep it off the runtime.
        let target_for_register = Arc::clone(&target);
        let size = self.size;
        let prefix_armed = Arc::clone(self.prefix_armed);
        let remote = self.remote;
        let Ok(Ok(new_handle)) = task::spawn_blocking(move || {
            target_for_register.register_client(size, prefix_armed, remote)
        })
        .await
        else {
            self.session
                .set_status_error(format!("cannot switch to {name}"))
                .await;
            return false;
        };
        // Re-point the renderer (rebind + invalidate + full repaint).
        let _ = self.switch_tx.send(new_handle.frame_rx.clone());
        let old = mem::replace(self.session, target);
        let old_id = mem::replace(self.client_id, new_handle.client_id);
        let _ = task::spawn_blocking(move || old.deregister_client(old_id)).await;
        self.session
            .set_status_info(format!("switched to {name}"))
            .await;
        true
    }
}

/// Snapshot the live sessions (sorted by name, current one marked) and open the
/// session picker. Shared by the `Ctrl+a w` keymap arm and the `:sessions`
/// command-prompt verb.
async fn open_session_picker_overlay(session: &Arc<Session>, registry: &Arc<SessionRegistry>) {
    let current = session.name();
    let mut entries: Vec<plexy_glass_mux::PickerEntry> = registry
        .list()
        .await
        .into_iter()
        .map(|e| {
            let label = format!(
                "{} \u{2014} {} win, {} panes, {} clients",
                e.name, e.windows, e.panes, e.clients
            );
            let is_current = e.name == current;
            plexy_glass_mux::PickerEntry {
                name: e.name,
                label,
                is_current,
            }
        })
        .collect();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    {
        let mut m = session.window_manager.lock().await;
        m.open_session_picker(entries);
    }
    session.notify.notify_one();
}

/// Snapshot every live session's windows/panes and open the choose-tree overlay.
/// Shared by the `Ctrl+a W` keymap arm and the `:tree` verb. Each session's
/// snapshot is taken via the async `tree_snapshot` (never `blocking_lock` from
/// this runtime task).
async fn open_tree_overlay(session: &Arc<Session>, registry: &Arc<SessionRegistry>) {
    let current = session.name();
    let mut snaps: Vec<SessionTree> = Vec::new();
    for entry in registry.list().await {
        let Some(s) = registry.get(&entry.name).await else {
            continue;
        };
        snaps.push(s.tree_snapshot().await);
    }
    let nodes = build_tree_nodes(&snaps, &current);
    {
        let mut m = session.window_manager.lock().await;
        m.open_tree(nodes);
    }
    session.notify.notify_one();
}

/// Assemble the flat `TreeNode` list (pre-order DFS: session → windows → panes)
/// from per-session snapshots. Pure so the `is_current`/label/index logic is
/// unit-testable. Only the current session's path is marked `is_current`: the
/// session itself, its active window, and that window's active pane.
fn build_tree_nodes(snaps: &[SessionTree], current: &str) -> Vec<plexy_glass_mux::TreeNode> {
    use plexy_glass_mux::{TreeNode, pane_label, session_label, window_label};
    let mut nodes: Vec<TreeNode> = Vec::new();
    for st in snaps {
        let is_cur = st.name == current;
        nodes.push(TreeNode {
            session: st.name.clone(),
            window: None,
            pane: None,
            depth: 0,
            label: session_label(&st.name, st.windows.len(), st.total_panes),
            name: st.name.clone(),
            index: 0,
            is_current: is_cur,
        });
        for (wi, w) in st.windows.iter().enumerate() {
            let widx = (wi as u32) + 1;
            nodes.push(TreeNode {
                session: st.name.clone(),
                window: Some(w.id),
                pane: None,
                depth: 1,
                label: window_label(widx, &w.name),
                name: w.name.clone(),
                index: widx,
                is_current: is_cur && wi == st.active_window,
            });
            for (pi, (pid, pname)) in w.panes.iter().enumerate() {
                let pidx = (pi as u32) + 1;
                let nm = pname.clone().unwrap_or_default();
                nodes.push(TreeNode {
                    session: st.name.clone(),
                    window: Some(w.id),
                    pane: Some(*pid),
                    depth: 2,
                    label: pane_label(pidx, &nm),
                    name: nm,
                    index: pidx,
                    is_current: is_cur && wi == st.active_window && *pid == w.active_pane,
                });
            }
        }
    }
    nodes
}

/// Cap on the history-palette corpus (newest-first, so the cap drops the oldest).
const HISTORY_ENTRY_CAP: usize = 2000;

/// Open the structured history palette: walk the registry, snapshot every
/// session's blocks, assemble a flat entry list (current pane first), and open
/// the overlay. Mirrors `open_tree_overlay`.
async fn open_history_overlay(session: &Arc<Session>, registry: &Arc<SessionRegistry>) {
    let current = session.name();
    let current_pane = session.active_pane_id().await;
    let mut snaps: Vec<SessionHistory> = Vec::new();
    for entry in registry.list().await {
        let Some(s) = registry.get(&entry.name).await else {
            continue;
        };
        snaps.push(s.history_snapshot().await);
    }
    let entries = build_history_entries(&snaps, &current, current_pane);
    // Empty corpus (no command blocks anywhere) is a different state from a
    // filter that matched nothing, so say so rather than opening a palette that
    // reads as "your search found nothing". Mirrors hint mode's empty handling.
    if entries.is_empty() {
        session.set_status_info(NO_BLOCKS_MSG.into()).await;
        return;
    }
    {
        let mut m = session.window_manager.lock().await;
        m.open_history(entries);
    }
    session.notify.notify_one();
}

/// Scan the active pane's visible grid for hint targets and open the hint-mode
/// overlay. Flashes "no hint targets" on the status line when nothing is found.
async fn open_hints_overlay(session: &Arc<Session>) {
    let cfg = session.config_snapshot();
    if !cfg.hints.enabled {
        return;
    }
    let alphabet = hint::effective_alphabet(&cfg.hints.alphabet);
    let targets = {
        let m = session.window_manager.lock().await;
        match m.active_window().active_pane() {
            Some(pane) => pane.with_screen(plexy_glass_mux::scan_hints),
            None => Vec::new(),
        }
    };
    if targets.is_empty() {
        session.set_status_info("no hint targets".into()).await;
        return;
    }
    let state = plexy_glass_mux::HintState::new(targets, &alphabet);
    {
        let mut m = session.window_manager.lock().await;
        m.open_hints(state);
    }
    session.notify.notify_one();
}

/// Build the palette catalog for the attached client: the static catalog with
/// each entry's key resolved from the active keymap via its `binding_verb`.
fn build_palette_entries(config: &plexy_glass_config::Config) -> Vec<PaletteEntry> {
    let keys = binding_keys(config);
    let mut entries = palette::catalog();
    for e in &mut entries {
        if let Some(v) = e.binding_verb {
            e.key = keys.get(v).cloned();
        }
    }
    entries
}

/// Build the catalog (keys resolved from the active keymap) and open the palette.
async fn open_palette_overlay(session: &Arc<Session>) {
    let cfg = session.config_snapshot();
    let entries = build_palette_entries(&cfg);
    {
        let mut m = session.window_manager.lock().await;
        m.open_palette(entries);
    }
    session.notify.notify_one();
}

/// Flatten per-session block snapshots into palette entries, ordered: the
/// current pane's blocks first, then the rest of the current session, then other
/// sessions, each pane's blocks already newest-first from `history_snapshot`.
/// Capped at [`HISTORY_ENTRY_CAP`] (logged when it triggers). Pure, for testing.
fn build_history_entries(
    snaps: &[SessionHistory],
    current_session: &str,
    current_pane: Option<plexy_glass_mux::PaneId>,
) -> Vec<plexy_glass_mux::HistoryEntry> {
    use plexy_glass_mux::HistoryEntry;
    let mut ranked: Vec<(u8, HistoryEntry)> = Vec::new();
    for st in snaps {
        let in_current_session = st.name == current_session;
        for b in &st.blocks {
            let rank = if in_current_session && Some(b.pane) == current_pane {
                0
            } else if in_current_session {
                1
            } else {
                2
            };
            ranked.push((
                rank,
                HistoryEntry {
                    session: st.name.clone(),
                    window: b.window,
                    window_idx: b.window_idx,
                    pane: b.pane,
                    prompt_line: b.prompt_line,
                    command: b.command.clone(),
                    exit: b.exit,
                    duration: b.duration,
                    haystack: b.haystack.clone(),
                },
            ));
        }
    }
    // Stable sort by rank only, so within a rank the snapshot order
    // (newest-first per pane) is preserved.
    ranked.sort_by_key(|(r, _)| *r);
    if ranked.len() > HISTORY_ENTRY_CAP {
        tracing::info!(
            total = ranked.len(),
            cap = HISTORY_ENTRY_CAP,
            "history palette truncated"
        );
        ranked.truncate(HISTORY_ENTRY_CAP);
    }
    ranked.into_iter().map(|(_, e)| e).collect()
}

impl ClientCtx<'_> {
    /// Perform a choose-tree action. `Switch` re-points this client (and focuses
    /// the chosen window/pane); the `Kill*`/`Rename*` actions reach into the
    /// target session via the registry. The current session is always notified
    /// afterward so its still-open overlay repaints the optimistic model update.
    async fn dispatch_tree_action(&mut self, action: plexy_glass_mux::TreeAction) {
        use plexy_glass_mux::TreeAction;
        match action {
            TreeAction::Switch {
                session: tgt,
                window,
                pane,
            } => {
                // Only focus the chosen window/pane when the client is actually
                // on the target session: either it was already there, or the
                // switch succeeded. On a failed switch the client stays on the
                // SOURCE, and because ids are not unique across sessions,
                // applying the target's ids here would mutate the wrong session.
                let on_target = if tgt == self.session.name() {
                    true
                } else {
                    self.switch_session(tgt).await
                };
                if on_target {
                    let mut m = self.session.window_manager.lock().await;
                    if let Some(w) = window {
                        m.select_window_by_id(w);
                    }
                    if let Some(p) = pane {
                        m.focus_pane_by_id(p);
                    }
                }
                self.session.notify.notify_one();
            }
            TreeAction::KillSession(name) => {
                match self.registry.kill(&name).await {
                    Ok(()) => self.session.set_status_ok(format!("killed {name}")).await,
                    Err(e) => self.session.set_status_error(e.to_string()).await,
                }
                self.session.notify.notify_one();
            }
            TreeAction::KillWindow {
                session: tgt,
                window,
            } => {
                if let Some(t) = self.registry.get(&tgt).await {
                    {
                        let mut m = t.window_manager.lock().await;
                        m.kill_window_panes(window);
                    }
                    t.notify.notify_one();
                } else {
                    self.session
                        .set_status_error(format!("no session: {tgt}"))
                        .await;
                }
                self.session.notify.notify_one();
            }
            TreeAction::KillPane { session: tgt, pane } => {
                if let Some(t) = self.registry.get(&tgt).await {
                    {
                        let mut m = t.window_manager.lock().await;
                        m.kill_pane_child(pane);
                    }
                    t.notify.notify_one();
                } else {
                    self.session
                        .set_status_error(format!("no session: {tgt}"))
                        .await;
                }
                self.session.notify.notify_one();
            }
            TreeAction::RenameWindow {
                session: tgt,
                window,
                name,
            } => {
                if let Some(t) = self.registry.get(&tgt).await {
                    {
                        let mut m = t.window_manager.lock().await;
                        m.rename_window_by_id(window, name);
                    }
                    t.notify.notify_one();
                } else {
                    self.session
                        .set_status_error(format!("no session: {tgt}"))
                        .await;
                }
                self.session.notify.notify_one();
            }
            TreeAction::RenamePane {
                session: tgt,
                pane,
                name,
            } => {
                if let Some(t) = self.registry.get(&tgt).await {
                    {
                        let mut m = t.window_manager.lock().await;
                        m.rename_pane_by_id(pane, name);
                    }
                    t.notify.notify_one();
                } else {
                    self.session
                        .set_status_error(format!("no session: {tgt}"))
                        .await;
                }
                self.session.notify.notify_one();
            }
            TreeAction::RenameSession { old, new } => {
                // Refuse renames TO a config-declared name. The template is
                // rebuilt fresh under that name at every daemon boot
                // (attach_or_create routes declared names to the template,
                // never the saved file), so the renamed session's persisted
                // state would be silently shadowed, and a hard no now beats
                // silent data loss later. (The FROM direction, renaming a
                // declared session away, stays allowed and merely decouples
                // it from its template, per the spec.)
                let cfg = self.session.config_snapshot();
                if cfg.sessions.iter().any(|t| t.name == new) {
                    self.session
                        .set_status_error(format!(
                            "'{new}' is a declared session name — choose another"
                        ))
                        .await;
                    self.session.notify.notify_one();
                    return;
                }
                match self.registry.rename_session(&old, &new).await {
                    Ok(()) => {
                        // Commit-on-success: the tree model was NOT mutated at
                        // commit time (session identity is stamped on every
                        // descendant row and in collapsed keys), so re-stamp
                        // the still-open overlay now.
                        let mut m = self.session.window_manager.lock().await;
                        m.rename_tree_session(&old, &new);
                    }
                    Err(e) => self.session.set_status_error(e.to_string()).await,
                }
                self.session.notify.notify_one();
            }
        }
    }

    /// Perform a history-palette jump: switch to the target session (if needed,
    /// like choose-tree's `Switch`), focus its window+pane, and enter block mode
    /// on the chosen block. The block is re-found at jump time by command (then
    /// nearest prompt) so scrollback drift since the palette opened can't land us
    /// on the wrong block; a vanished target flashes a status message.
    async fn dispatch_history_jump(&mut self, target: plexy_glass_mux::HistoryTarget) {
        use plexy_glass_mux::{BlockMode, blocks};
        let on_target = if target.session == self.session.name() {
            true
        } else {
            self.switch_session(target.session.clone()).await
        };
        let landed = if on_target {
            let mut m = self.session.window_manager.lock().await;
            m.select_window_by_id(target.window);
            m.focus_pane_by_id(target.pane);
            match m.active_window().pane(target.pane) {
                Some(pane) => {
                    let state = pane.with_screen(|s| {
                        let line =
                            blocks::find_block_by_command(s, &target.command, target.prompt_line)
                                .or_else(|| blocks::prompt_at_or_above(s, target.prompt_line))
                                .or_else(|| blocks::first_prompt_line(s));
                        line.and_then(|l| BlockMode::new_at(s, s.active.num_rows(), l))
                    });
                    match state {
                        Some(state) => {
                            pane.enter_block_mode(state);
                            true
                        }
                        None => false,
                    }
                }
                None => false,
            }
        } else {
            false
        };
        if !landed {
            self.session
                .set_status_error("history: block no longer available".into())
                .await;
        }
        self.session.notify.notify_one();
    }

    /// Perform a hint-mode pick: copy the span's text to the system clipboard
    /// and push it as a paste buffer (mirroring copy-mode yank), or open the
    /// span's URL/path in the system opener (fire-and-forget).
    async fn dispatch_hint(&self, pick: plexy_glass_mux::HintPick) {
        match pick.action {
            plexy_glass_mux::HintAction::Copy => {
                // Mirror the copy-mode yank path: system clipboard + paste buffer.
                let wrote = osc_actions::write_clipboard(pick.text.as_bytes()).await;
                let msg = osc_actions::copied_message(&pick.text);
                self.registry
                    .push_paste_buffer(pick.text.into_bytes())
                    .await;
                // Be honest: only claim "copied" if the OS clipboard write landed;
                // otherwise the content is still in the paste buffer, so point there.
                if wrote {
                    self.session.set_status_ok(msg).await;
                } else {
                    self.session
                        .set_status_warn("clipboard unavailable — paste with Ctrl+a ]".into())
                        .await;
                }
            }
            plexy_glass_mux::HintAction::Open => {
                // Await the opener (not fire-and-forget) so a missing system
                // opener becomes a visible message instead of a silent no-op.
                let url = pick.text;
                match osc_actions::open_url(&url).await {
                    Ok(()) => self.session.set_status_info(format!("opening {url}")).await,
                    Err(_) => {
                        self.session
                            .set_status_error("couldn't open (no system opener)".into())
                            .await;
                    }
                }
            }
        }
        self.session.notify.notify_one();
    }
}

/// A command verb handled at the connection layer rather than inside the
/// session: it needs this connection's context (registry, renderer switch
/// channel, keymap). Both the keymap `Command` arm and the command-prompt
/// `PromptCommand` arm map into this, so each verb's body exists exactly once
/// (`run_connection_verb`). Note that the matching placeholder arms in
/// `WindowManager::handle_command` stay, they are load-bearing for match
/// exhaustiveness.
enum ConnVerb {
    Detach,
    Reload,
    Switch(String),
    ChooseSession,
    ChooseTree,
    History,
    Hints,
    CommandPalette,
    PasteBuffer(Option<String>),
    ChooseBuffer,
    CopyOutput,
    EnterBlockMode,
    SetBuffer(String),
    SaveBuffer { name: Option<String>, path: String },
    LoadBuffer(String),
}

impl ConnVerb {
    /// Keymap commands handled at the connection layer. Everything else is
    /// returned unchanged for the caller (`CommandPrompt` opener or
    /// `Session::handle_command`). The keymap has no by-name switch binding,
    /// so `Switch` only arises from `from_prompt`; likewise the buffer-file
    /// verbs (set/save/load) and by-name paste are prompt-only.
    fn from_command(cmd: Command) -> Result<Self, Command> {
        match cmd {
            Command::Detach => Ok(Self::Detach),
            Command::ReloadConfig => Ok(Self::Reload),
            Command::ChooseSession => Ok(Self::ChooseSession),
            Command::ChooseTree => Ok(Self::ChooseTree),
            Command::History => Ok(Self::History),
            Command::Hints => Ok(Self::Hints),
            Command::CommandPalette => Ok(Self::CommandPalette),
            Command::PasteBuffer => Ok(Self::PasteBuffer(None)),
            Command::ChooseBuffer => Ok(Self::ChooseBuffer),
            Command::CopyOutput => Ok(Self::CopyOutput),
            Command::EnterBlockMode => Ok(Self::EnterBlockMode),
            other => Err(other),
        }
    }

    /// Prompt commands handled at the connection layer. Everything else is
    /// returned unchanged for `Session::handle_prompt_command`.
    /// Lockstep: any verb added here must also be handled in `run_prompt_line`
    /// (see `tests::run_prompt_line_never_silently_noops_connection_verbs`).
    fn from_prompt(cmd: PromptCommand) -> Result<Self, PromptCommand> {
        match cmd {
            PromptCommand::Detach => Ok(Self::Detach),
            PromptCommand::Reload => Ok(Self::Reload),
            PromptCommand::Switch(name) => Ok(Self::Switch(name)),
            PromptCommand::ChooseSession => Ok(Self::ChooseSession),
            PromptCommand::ChooseTree => Ok(Self::ChooseTree),
            PromptCommand::History => Ok(Self::History),
            PromptCommand::Hints => Ok(Self::Hints),
            PromptCommand::CommandPalette => Ok(Self::CommandPalette),
            PromptCommand::PasteBuffer(name) => Ok(Self::PasteBuffer(name)),
            PromptCommand::ChooseBuffer => Ok(Self::ChooseBuffer),
            PromptCommand::CopyOutput => Ok(Self::CopyOutput),
            PromptCommand::BlockMode => Ok(Self::EnterBlockMode),
            PromptCommand::SetBuffer { text } => Ok(Self::SetBuffer(text)),
            PromptCommand::SaveBuffer { name, path } => Ok(Self::SaveBuffer { name, path }),
            PromptCommand::LoadBuffer { path } => Ok(Self::LoadBuffer(path)),
            other => Err(other),
        }
    }
}

/// The attach-time status notice (the first-run onboarding moved to the welcome
/// modal). A broken config (running defaults) outranks a dropped-binding
/// warning. Pure, so the precedence is unit-testable.
fn attach_notice(has_config_error: bool, skip_count: usize) -> Option<(Severity, String)> {
    use Severity;
    if has_config_error {
        Some((
            Severity::Error,
            "config error — running defaults; run plexy-glass reload for details".to_string(),
        ))
    } else if skip_count > 0 {
        Some((
            Severity::Warn,
            format!("{skip_count} keymap binding(s) skipped — see plexy-glass reload"),
        ))
    } else {
        None
    }
}

/// The status notice after a `reload`: the error wins; otherwise a clean reload
/// reports success, noting any dropped bindings. Pure, for unit-testing.
fn reload_notice(error: Option<&str>, skip_count: usize) -> (Severity, String) {
    use Severity;
    match error {
        Some(e) => (Severity::Error, format!("reload failed: {e}")),
        None if skip_count > 0 => (
            Severity::Warn,
            format!("config reloaded · {skip_count} binding(s) skipped"),
        ),
        None => (Severity::Success, "config reloaded".to_string()),
    }
}

/// Execute a connection-layer verb. Returns `true` for `Detach`, which the
/// caller (who owns the input loop) must translate into its own `break`.
async fn run_connection_verb(ctx: &mut ClientCtx<'_>, keymap: &mut Keymap, verb: ConnVerb) -> bool {
    match verb {
        ConnVerb::Detach => return true,
        ConnVerb::Reload => {
            let result = ctx.registry.reload_config().await;
            // Rebuild this Connection's keymap from the session's current config
            // so the user who fired the reload sees binding changes immediately
            // and we can report any dropped bindings. On a clean reload that's
            // the freshly-swapped config; on a failed one it's the retained
            // last-known-good (unchanged), so either way read it back and rebuild.
            let new_cfg = ctx.session.config_snapshot();
            let (km, skips) = plexy_glass_keys::build_keymap_with_skips(&new_cfg.keymap);
            *keymap = km;
            let err_text = result.as_ref().err().map(ToString::to_string);
            let (severity, text) = reload_notice(err_text.as_deref(), skips.len());
            ctx.session.set_status_message(text, severity).await;
            ctx.session.notify.notify_one();
        }
        ConnVerb::Switch(name) => {
            ctx.switch_session(name).await;
        }
        ConnVerb::ChooseSession => {
            if ctx.version >= 12 {
                // v12+ client renders its own picker; hand it our session list.
                let sessions = ctx.registry.list().await;
                let current = ctx.session.name();
                let _ = ctx
                    .inject_tx
                    .send(RenderInject::Msg(ServerMsg::OpenSessionPicker {
                        sessions,
                        current,
                    }));
            } else {
                open_session_picker_overlay(ctx.session, ctx.registry).await;
            }
        }
        ConnVerb::ChooseTree => {
            open_tree_overlay(ctx.session, ctx.registry).await;
        }
        ConnVerb::History => {
            open_history_overlay(ctx.session, ctx.registry).await;
        }
        ConnVerb::Hints => {
            open_hints_overlay(ctx.session).await;
        }
        ConnVerb::CommandPalette => {
            open_palette_overlay(ctx.session).await;
        }
        ConnVerb::PasteBuffer(None) => {
            paste_top_buffer(ctx.session, ctx.registry).await;
        }
        ConnVerb::PasteBuffer(Some(name)) => {
            if let Err(e) = paste_named_buffer(ctx.session, ctx.registry, &name).await {
                ctx.session.set_status_error(e).await;
            }
        }
        ConnVerb::ChooseBuffer => {
            open_buffer_picker_overlay(ctx.session, ctx.registry).await;
        }
        ConnVerb::CopyOutput => {
            // Status messages (success and no-blocks) are set inside.
            let _ = copy_last_output(ctx.session, ctx.registry).await;
        }
        ConnVerb::EnterBlockMode => {
            enter_block_mode(ctx.session).await;
        }
        ConnVerb::SetBuffer(text) => {
            let msg = set_buffer(ctx.registry, text).await;
            ctx.session.set_status_ok(msg).await;
        }
        ConnVerb::SaveBuffer { name, path } => {
            // Both arms carry the status-line text; the severity follows.
            match save_buffer(ctx.registry, name, &path).await {
                Ok(m) => ctx.session.set_status_ok(m).await,
                Err(m) => ctx.session.set_status_error(m).await,
            }
        }
        ConnVerb::LoadBuffer(path) => match load_buffer(ctx.registry, &path).await {
            Ok(m) => ctx.session.set_status_ok(m).await,
            Err(m) => ctx.session.set_status_error(m).await,
        },
    }
    false
}

/// Dispatch a parsed `PromptCommand` exactly as a committed command-prompt line:
/// connection-layer verbs go through `run_connection_verb`, the rest through
/// `Session::handle_prompt_command`. Returns `true` on a detach request.
async fn dispatch_prompt_command(
    ctx: &mut ClientCtx<'_>,
    keymap: &mut Keymap,
    cmd: PromptCommand,
) -> bool {
    match ConnVerb::from_prompt(cmd) {
        Ok(verb) => run_connection_verb(ctx, keymap, verb).await,
        Err(other) => {
            match ctx.session.handle_prompt_command(other).await {
                Ok(Some(msg)) => ctx.session.set_status_info(msg).await,
                Ok(None) => {}
                Err(e) => ctx.session.set_status_error(e.to_string()).await,
            }
            false
        }
    }
}

/// Apply the result of one key delivered to an open overlay, the block that
/// grows with every new overlay, extracted so the input loop stays shallow.
/// Returns `true` when the result requested a detach (`:detach` from the
/// command prompt); the caller owns the loop and must `break` on it.
async fn apply_overlay_result(
    ctx: &mut ClientCtx<'_>,
    keymap: &mut Keymap,
    result: OverlayKeyResult,
) -> bool {
    use OverlayKeyResult;
    match result {
        OverlayKeyResult::Ignored => {}
        OverlayKeyResult::Redraw => {
            ctx.session.notify.notify_one();
        }
        OverlayKeyResult::Committed => {
            // A rename changed a window/pane name: redraw.
            ctx.session.notify.notify_one();
        }
        OverlayKeyResult::SwitchSession(name) => {
            ctx.switch_session(name).await;
        }
        OverlayKeyResult::Tree(action) => {
            ctx.dispatch_tree_action(action).await;
        }
        OverlayKeyResult::History(target) => {
            ctx.dispatch_history_jump(target).await;
        }
        OverlayKeyResult::Hint(pick) => {
            ctx.dispatch_hint(pick).await;
        }
        OverlayKeyResult::Buffer(action) => {
            use plexy_glass_mux::BufferAction;
            match action {
                BufferAction::Paste(name) => {
                    if let Some(content) = ctx.registry.paste_buffer_get(&name).await {
                        paste_bytes(ctx.session, content).await;
                    } else {
                        // The overlay closed; repaint it away even on a
                        // get() miss.
                        ctx.session.notify.notify_one();
                    }
                }
                BufferAction::Delete(name) => {
                    ctx.registry.delete_paste_buffer(&name).await;
                    // Repaint the still-open overlay.
                    ctx.session.notify.notify_one();
                }
            }
        }
        OverlayKeyResult::PaletteRun(cmd) => {
            return dispatch_prompt_command(ctx, keymap, cmd).await;
        }
        OverlayKeyResult::PalettePrompt(prefill) => {
            let names: Vec<String> = ctx
                .registry
                .list()
                .await
                .into_iter()
                .map(|e| e.name)
                .collect();
            {
                let mut m = ctx.session.window_manager.lock().await;
                m.open_command_prompt_prefilled(names, prefill);
            }
            ctx.session.notify.notify_one();
        }
        OverlayKeyResult::Command(line) => match command_prompt::parse(&line) {
            Err(e) => {
                ctx.session.set_status_error(e.to_string()).await;
            }
            Ok(cmd) => return dispatch_prompt_command(ctx, keymap, cmd).await,
        },
    }
    false
}

/// Snapshot the paste buffers (newest-first) and open the choose-buffer overlay.
/// Shared by `Ctrl+a =` and the `:buffers` verb.
async fn open_buffer_picker_overlay(session: &Arc<Session>, registry: &Arc<SessionRegistry>) {
    let entries = registry.list_paste_buffers().await;
    {
        let mut m = session.window_manager.lock().await;
        m.open_buffer_picker(entries);
    }
    session.notify.notify_one();
}

/// Resolve the target session for a one-shot scripting message (CLI
/// `cmd`/`send`/`capture`): an explicit name must exist; with no name there
/// must be exactly one running session. The error string goes back to the CLI
/// in `CommandResult`.
async fn resolve_session(
    registry: &Arc<SessionRegistry>,
    name: Option<String>,
) -> Result<Arc<Session>, String> {
    if let Some(n) = name {
        registry
            .get(&n)
            .await
            .ok_or_else(|| format!("no session \"{n}\""))
    } else {
        let entries = registry.list().await;
        match entries.as_slice() {
            [] => Err("no sessions running".into()),
            [only] => registry
                .get(&only.name)
                .await
                .ok_or_else(|| format!("no session \"{}\"", only.name)),
            many => {
                let names = many
                    .iter()
                    .map(|e| e.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                Err(format!("multiple sessions running: {names} — use -n"))
            }
        }
    }
}

/// Precondition snapshot for [`serve_exec`], read in ONE `with_screen`
/// closure: the pane's emulator mutex is the fence between the at-prompt
/// check and the counter baseline, so the reader thread cannot process a `D`
/// in between (the spec's fencing-honesty note).
enum ExecPre {
    AltScreen,
    NoMarks,
    Busy,
    Ready { baseline: u64 },
}

/// One counter poll against the injection baseline during the exec wait.
enum ExecTick {
    Pending,
    /// Counter went backwards: the screen was rebuilt (RIS) mid-command.
    Reset,
    Done {
        exit: Option<i32>,
        output: String,
    },
}

/// Serve one `ExecCommand` (CLI `run`): check preconditions and read the
/// completed-block baseline in a single screen closure, inject `text` + `\r`
/// directly into the input target pane, then wait for the pane's
/// completed-block counter to pass the baseline, racing the pane child's
/// exit, the optional timeout, and the client connection itself (a dropped
/// client abandons the wait silently; no reply).
///
/// Response-type asymmetry by design (like `CaptureLastCommand`): completion
/// and timeout reply `ExecDone`; every refusal (no session, no pane, no
/// blocks, busy, alt screen, child exit, mid-command reset, unexpected frame)
/// replies `CommandResult { ok: false }`, and the CLI client matches on
/// either.
async fn serve_exec<R, W>(
    reader: &mut R,
    writer: &mut W,
    registry: &Arc<SessionRegistry>,
    session: Option<String>,
    text: String,
    timeout_ms: Option<u64>,
) -> Result<(), DaemonError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let refuse = |message: String| ServerMsg::CommandResult {
        ok: false,
        message: Some(message),
    };

    let sess = match resolve_session(registry, session).await {
        Err(msg) => return send_msg(writer, &refuse(msg)).await,
        Ok(s) => s,
    };
    let pane = {
        let manager = sess.window_manager.lock().await;
        manager.input_target_pane().cloned()
    };
    let Some(pane) = pane else {
        // Unreachable in practice: a session with no panes tears itself down.
        return send_msg(writer, &refuse("no focused pane".into())).await;
    };

    // Preconditions AND baseline in one closure (see `ExecPre`).
    let pre = pane.with_screen(|s| {
        if s.alt.is_some() {
            return ExecPre::AltScreen;
        }
        // Any PROMPT_START anywhere (scrollback + grid)? Without shell
        // integration no `D` will ever arrive, so refuse fast instead of
        // hanging. (`pane_at_prompt` alone can't distinguish this case.)
        if plexy_glass_mux::prev_prompt_line(s, u32::MAX).is_none() {
            return ExecPre::NoMarks;
        }
        if !blocks::pane_at_prompt(s) {
            return ExecPre::Busy;
        }
        ExecPre::Ready {
            baseline: s.blocks_completed,
        }
    });
    let baseline = match pre {
        ExecPre::AltScreen => {
            return send_msg(
                writer,
                &refuse("pane is busy: alternate screen is active".into()),
            )
            .await;
        }
        ExecPre::NoMarks => return send_msg(writer, &refuse(NO_BLOCKS_MSG.into())).await,
        ExecPre::Busy => {
            return send_msg(writer, &refuse("pane is busy: a command is running".into())).await;
        }
        ExecPre::Ready { baseline } => baseline,
    };

    // Inject directly to the target pane, NOT `Session::handle_input_bytes`
    // (that is the sync-panes fan-out, and a synchronized multi-pane run has
    // no single answer), then wake the render coordinator the same way
    // `handle_input_bytes` does.
    let mut bytes = text.into_bytes();
    bytes.push(b'\r');
    if pane.send_input(bytes::Bytes::from(bytes)).await.is_err() {
        return send_msg(writer, &refuse("run: pane input channel closed".into())).await;
    }
    sess.notify.notify_one();

    // The wait. Every long-lived future is created ONCE and polled across
    // loop iterations: `Codec::read_frame` is `read_exact`-based (NOT
    // cancel-safe, dropping it mid-frame loses buffered bytes), so it is
    // pinned outside the loop instead of recreated per tick. That is
    // sufficient because every way it completes ends the wait.
    let mut interval = time::interval(Duration::from_millis(25));
    let exit_fut = pane.wait();
    tokio::pin!(exit_fut);
    let timeout_fut = async {
        match timeout_ms {
            Some(ms) => time::sleep(Duration::from_millis(ms)).await,
            // No timeout requested: this arm never fires.
            None => future::pending::<()>().await,
        }
    };
    tokio::pin!(timeout_fut);
    let read_fut = plexy_glass_protocol::Codec::read_frame(reader);
    tokio::pin!(read_fut);

    // One closure for counter + exit + output so there's no gap between
    // observing completion and reading the block text. It's also the FINAL
    // check in the child-exit and timeout arms, because a command whose D
    // was processed just before its pane died (or the deadline fired) is a
    // completion, not a failure; without this a finished command could be
    // misreported within one poll interval.
    let check = || {
        pane.with_screen(|s| match s.blocks_completed.cmp(&baseline) {
            CmpOrdering::Greater => {
                // Output is best-effort from surviving rows: the command
                // may have cleared the screen (empty is fine).
                let output = plexy_glass_mux::last_completed_block(s)
                    .map(|range| plexy_glass_mux::block_text(s, range))
                    .unwrap_or_default();
                ExecTick::Done {
                    exit: s.last_block_exit,
                    output,
                }
            }
            CmpOrdering::Less => ExecTick::Reset,
            CmpOrdering::Equal => ExecTick::Pending,
        })
    };

    let reply = loop {
        tokio::select! {
            _ = interval.tick() => {
                match check() {
                    ExecTick::Pending => {}
                    ExecTick::Reset => break refuse("run: pane was reset mid-command".into()),
                    ExecTick::Done { exit, output } => {
                        break ServerMsg::ExecDone { exit, output, timed_out: false };
                    }
                }
            }
            _ = &mut exit_fut => match check() {
                ExecTick::Done { exit, output } => {
                    break ServerMsg::ExecDone { exit, output, timed_out: false };
                }
                _ => break refuse("run: pane child exited".into()),
            },
            () = &mut timeout_fut => match check() {
                ExecTick::Done { exit, output } => {
                    break ServerMsg::ExecDone { exit, output, timed_out: false };
                }
                // Structural timeout, so the CLI maps it to exit 124. The
                // command is NOT killed; it is the user's session.
                _ => break ServerMsg::ExecDone {
                    exit: None,
                    output: String::new(),
                    timed_out: true,
                },
            },
            frame = &mut read_fut => match frame {
                // Client gone (e.g. Ctrl-C'd CLI): abandon the wait silently.
                Ok(None) | Err(_) => return Ok(()),
                Ok(Some(_)) => break refuse("run: unexpected message during wait".into()),
            },
        }
    };
    send_msg(writer, &reply).await
}

/// Run one command-prompt line headlessly (CLI `cmd`). Returns `(ok, message)`
/// for `CommandResult`. Mirrors the attached prompt's connection-layer
/// interception (`ConnVerb`), except verbs that act on the calling client
/// (detach/switch) or open modal overlays (help/sessions/tree/buffers) are
/// refused, since a one-shot connection has no attached client and opening UI
/// from a script would hijack whoever is attached.
async fn run_prompt_line(
    session: &Arc<Session>,
    registry: &Arc<SessionRegistry>,
    line: &str,
) -> (bool, Option<String>) {
    let cmd = match command_prompt::parse(line) {
        Ok(c) => c,
        Err(e) => return (false, Some(e.to_string())),
    };
    let refuse = |verb: &str| (false, Some(format!("{verb}: requires an attached client")));
    match cmd {
        PromptCommand::Reload => match registry.reload_config().await {
            Ok(()) => (true, None),
            Err(e) => (false, Some(format!("reload failed: {e}"))),
        },
        PromptCommand::PasteBuffer(None) => {
            paste_top_buffer(session, registry).await;
            (true, None)
        }
        PromptCommand::PasteBuffer(Some(name)) => {
            match paste_named_buffer(session, registry, &name).await {
                Ok(()) => (true, None),
                Err(e) => (false, Some(e)),
            }
        }
        PromptCommand::SetBuffer { text } => (true, Some(set_buffer(registry, text).await)),
        PromptCommand::SaveBuffer { name, path } => {
            match save_buffer(registry, name, &path).await {
                Ok(m) => (true, Some(m)),
                Err(e) => (false, Some(e)),
            }
        }
        PromptCommand::LoadBuffer { path } => match load_buffer(registry, &path).await {
            Ok(m) => (true, Some(m)),
            Err(e) => (false, Some(e)),
        },
        PromptCommand::CopyOutput => {
            if copy_last_output(session, registry).await {
                (true, None)
            } else {
                (false, Some(NO_BLOCKS_MSG.to_string()))
            }
        }
        // Block mode is interactive, per-pane modal navigation, so refuse
        // headlessly like the other attached-only verbs.
        PromptCommand::BlockMode => refuse("block-mode"),
        PromptCommand::Detach => refuse("detach"),
        PromptCommand::Switch(_) => refuse("switch"),
        PromptCommand::Help => refuse("help"),
        PromptCommand::ChooseSession => refuse("sessions"),
        PromptCommand::ChooseTree => refuse("tree"),
        PromptCommand::History => refuse("history"),
        PromptCommand::Hints => refuse("hints"),
        PromptCommand::CommandPalette => refuse("palette"),
        PromptCommand::ChooseBuffer => refuse("buffers"),
        other => match session.handle_prompt_command(other).await {
            Ok(message) => (true, message),
            Err(e) => (false, Some(e.to_string())),
        },
    }
}

/// Status text when `copy-output` finds no completed OSC 133 block. Shared
/// by the interactive status line and the headless `cmd` result.
const NO_BLOCKS_MSG: &str =
    "no command blocks — shell integration not active? see docs/command-blocks.md";

/// Copy the last completed command block's output (scrollback included) to
/// the clipboard and push it onto the paste-buffer stack. Reads the
/// input-target pane (the popup's child while one is open, otherwise the
/// active pane) for consistency with every other read/write surface
/// (paste, capture); interactively the distinction is moot because a popup
/// swallows the chord, but headless `cmd "copy-output"` can run while a
/// popup is open. Returns whether a block was found; sets the status message
/// either way. Shared by the `copy_output` binding verb and `:copy-output`.
async fn copy_last_output(session: &Arc<Session>, registry: &Arc<SessionRegistry>) -> bool {
    let text = {
        let manager = session.window_manager.lock().await;
        manager.input_target_pane().and_then(|p| {
            p.with_screen(|s| {
                plexy_glass_mux::last_completed_block(s)
                    .map(|range| plexy_glass_mux::block_text(s, range))
            })
        })
    };
    if let Some(text) = text {
        let _ = osc_actions::write_clipboard(text.as_bytes()).await;
        registry.push_paste_buffer(text.into_bytes()).await;
        session
            .set_status_ok("copied output of last command".into())
            .await;
        true
    } else {
        session.set_status_info(NO_BLOCKS_MSG.into()).await;
        false
    }
}

/// Open block mode on the active pane, or set the no-blocks status hint and
/// refuse. The newest block is selected. Reads the ACTIVE pane (block mode is
/// a focus-pane modal navigation; a popup swallows the entry chord upstream).
async fn enter_block_mode(session: &Arc<Session>) {
    let opened = {
        let manager = session.window_manager.lock().await;
        match manager.active_window().active_pane() {
            Some(pane) => {
                let state = pane
                    .with_screen(|s| plexy_glass_mux::BlockMode::new_for(s, s.active.num_rows()));
                match state {
                    Some(state) => {
                        pane.enter_block_mode(state);
                        true
                    }
                    None => false,
                }
            }
            None => false,
        }
    };
    if opened {
        session.notify.notify_one();
    } else {
        session
            .set_status_info("no command blocks in this pane".into())
            .await;
    }
}

/// Paste the most-recent paste buffer into the input-target pane (bracketed
/// if that pane requests it), or set a status when there is none. Shared by
/// `Ctrl+a ]` and the `:paste` verb.
async fn paste_top_buffer(session: &Arc<Session>, registry: &Arc<SessionRegistry>) {
    match registry.paste_buffer_top().await {
        Some(content) => paste_bytes(session, content).await,
        None => session.set_status_info("no paste buffer".into()).await,
    }
}

/// Paste the buffer named `name` (`:paste bufferN`). `Err` carries the
/// unknown-name text: the interactive path shows it as a status message,
/// and the headless path returns it in `CommandResult`.
async fn paste_named_buffer(
    session: &Arc<Session>,
    registry: &Arc<SessionRegistry>,
    name: &str,
) -> Result<(), String> {
    match registry.paste_buffer_get(name).await {
        Some(content) => {
            paste_bytes(session, content).await;
            Ok(())
        }
        None => Err(format!("paste: no buffer named {name}")),
    }
}

/// `load-buffer`'s size cap. Buffers are memory-resident and cloned per
/// paste, and the store bounds count, not bytes, so the system's only
/// arbitrary-file ingress is bounded here.
const LOAD_BUFFER_MAX_BYTES: u64 = 10 * 1024 * 1024;

/// Resolve a `save-buffer` / `load-buffer` path: expand a leading `~`
/// against `$HOME`, then refuse anything still relative. The daemon's cwd is
/// whatever directory the first auto-spawning client happened to be in
/// (undiscoverable from any plexy-glass surface), so relative resolution
/// would be a silent footgun.
fn resolve_buffer_path(verb: &str, path: &str) -> Result<PathBuf, String> {
    let home = env::var("HOME").ok();
    let resolved = PathBuf::from(expand_tilde(path, home.as_deref()));
    if resolved.is_relative() {
        return Err(format!(
            "{verb}: relative paths are not supported — the daemon's working \
             directory is not yours; use an absolute or ~ path"
        ));
    }
    Ok(resolved)
}

/// `:set-buffer <text…>` pushes literal text as a new newest paste buffer.
/// The grammar guarantees non-empty text, and a prompt line cannot carry
/// newlines and is edge-trimmed (use `load-buffer` for those).
async fn set_buffer(registry: &Arc<SessionRegistry>, text: String) -> String {
    let n = text.len();
    registry.push_paste_buffer(text.into_bytes()).await;
    format!("buffer set ({n} bytes)")
}

/// `:save-buffer [bufferN] <path…>` writes a buffer (named, or the newest) to `path`.
/// Bytes go out verbatim, truncate-overwrite; that's non-atomic by design, since
/// these are user export files, not state files. Success and error texts both
/// carry the resolved path.
async fn save_buffer(
    registry: &Arc<SessionRegistry>,
    name: Option<String>,
    path: &str,
) -> Result<String, String> {
    let resolved = resolve_buffer_path("save-buffer", path)?;
    let (buf_name, content) = match name {
        Some(n) => {
            let content = registry
                .paste_buffer_get(&n)
                .await
                .ok_or_else(|| format!("save-buffer: no buffer named {n}"))?;
            (n, content)
        }
        None => registry
            .paste_buffer_top_entry()
            .await
            .ok_or_else(|| "save-buffer: no paste buffer".to_string())?,
    };
    fs::write(&resolved, &content)
        .map_err(|e| format!("save-buffer: {}: {e}", resolved.display()))?;
    Ok(format!(
        "saved {buf_name} → {} ({} bytes)",
        resolved.display(),
        content.len()
    ))
}

/// `:load-buffer <path…>`: read a file into a new newest paste buffer.
/// This is the system's only arbitrary-file ingress, so it is gated BEFORE
/// reading: `metadata`-then-`is_file()` refuses FIFOs, devices, and
/// directories (a FIFO `open` would hang a runtime worker; `/dev/zero` would
/// OOM); symlinks to regular files are followed deliberately. The size cap
/// guards resident memory (buffers are resident and cloned per paste).
/// Empty files load as an empty buffer.
async fn load_buffer(registry: &Arc<SessionRegistry>, path: &str) -> Result<String, String> {
    let resolved = resolve_buffer_path("load-buffer", path)?;
    let disp = resolved.display();
    let meta = fs::metadata(&resolved).map_err(|e| format!("load-buffer: {disp}: {e}"))?;
    if !meta.is_file() {
        return Err(format!("load-buffer: {disp}: not a regular file"));
    }
    if meta.len() > LOAD_BUFFER_MAX_BYTES {
        return Err(format!(
            "load-buffer: {disp} is {} bytes (limit 10 MiB)",
            meta.len()
        ));
    }
    // Belt-and-braces cap: the file may grow between stat and read; take(cap+1)
    // limits the actual bytes read so a race cannot OOM. If the read fills the
    // cap+1 bucket the file is oversize and we return the oversize error.
    use std::io::Read as _;
    let mut content = Vec::new();
    fs::File::open(&resolved)
        .map_err(|e| format!("load-buffer: {disp}: {e}"))?
        .take(LOAD_BUFFER_MAX_BYTES + 1)
        .read_to_end(&mut content)
        .map_err(|e| format!("load-buffer: {disp}: {e}"))?;
    if content.len() as u64 > LOAD_BUFFER_MAX_BYTES {
        return Err(format!("load-buffer: {disp} exceeds the 10 MiB limit"));
    }
    let n = content.len();
    registry.push_paste_buffer(content).await;
    Ok(format!("loaded {disp} ({n} bytes)"))
}

/// Send `content` to the input target as a PASTE: `handle_input_bytes` wraps it
/// in bracketed-paste markers per receiving pane (each target's own `?2004`),
/// so under sync-panes a paste is bracketed correctly for every pane instead of
/// once from the active one. Shared by `InputEvent::Paste`, `Ctrl+a ]`, and the
/// choose-buffer paste action.
async fn paste_bytes(session: &Arc<Session>, content: Vec<u8>) {
    let _ = session.handle_input_bytes(&content, true).await;
}

fn default_spawn_spec() -> SpawnSpec {
    let program = env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    SpawnSpec {
        program,
        args: vec![],
        env: vec![],
        cwd: None,
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;
    use std::process;

    use plexy_glass_protocol::{PROTOCOL_VERSION, PtySize, SpawnSpec, client_handshake};
    use tokio::io::duplex;

    use super::*;
    use crate::pane::Pane;
    use crate::session::wrap_bracketed_paste;
    use crate::test_env::isolate;

    #[test]
    fn wrap_paste_wraps_with_bracketed_paste_escapes() {
        let inner = b"hello world";
        let wrapped = wrap_bracketed_paste(inner);
        assert_eq!(wrapped.as_slice(), b"\x1b[200~hello world\x1b[201~");
    }

    #[test]
    fn attach_notice_prefers_config_error_over_skips() {
        use Severity;
        // A broken config wins over a skip warning.
        let (sev, text) = attach_notice(true, 3).unwrap();
        assert_eq!(sev, Severity::Error);
        assert!(text.starts_with("config error"), "got {text:?}");
        // No error, but bindings were skipped → warn.
        let (sev, text) = attach_notice(false, 2).unwrap();
        assert_eq!(sev, Severity::Warn);
        assert_eq!(text, "2 keymap binding(s) skipped — see plexy-glass reload");
        // Clean config, no skips → no status notice (the welcome modal, if any,
        // is handled separately).
        assert!(attach_notice(false, 0).is_none());
    }

    #[test]
    fn reload_notice_reports_error_skips_or_clean_success() {
        use Severity;
        let (sev, text) = reload_notice(Some("line 7:3: boom"), 0);
        assert_eq!(sev, Severity::Error);
        assert_eq!(text, "reload failed: line 7:3: boom");
        let (sev, text) = reload_notice(None, 2);
        assert_eq!(sev, Severity::Warn);
        assert_eq!(text, "config reloaded · 2 binding(s) skipped");
        let (sev, text) = reload_notice(None, 0);
        assert_eq!(sev, Severity::Success);
        assert_eq!(text, "config reloaded");
    }

    #[test]
    fn wrap_paste_empty_input() {
        let wrapped = wrap_bracketed_paste(b"");
        assert_eq!(wrapped.as_slice(), b"\x1b[200~\x1b[201~");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn end_to_end_attach_renders_then_exits() {
        let _g = isolate();
        let (server_side, client_side) = duplex(64 * 1024);
        let server = tokio::spawn(async move {
            serve(
                server_side,
                7,
                Arc::new(crate::SessionRegistry::new()),
                Arc::new(plexy_glass_config::built_in_default()),
            )
            .await
        });

        let (mut cr, mut cw) = io::split(client_side);
        let server_hello = client_handshake(&mut cr, &mut cw).await.unwrap();
        assert_eq!(server_hello.version, PROTOCOL_VERSION);

        let attach = ClientMsg::AttachOrCreate {
            name: Some("test".into()),
            create_if_missing: true,
            cmd: Some(SpawnSpec {
                program: "/bin/echo".into(),
                args: vec!["hi".into()],
                env: vec![],
                cwd: None,
            }),
            size: PtySize {
                rows: 8,
                cols: 24,
                pixel_width: 0,
                pixel_height: 0,
            },
        };
        let bytes = postcard::to_allocvec(&attach).unwrap();
        Codec::write_frame(&mut cw, &bytes).await.unwrap();

        let mut saw_attached = false;
        let mut saw_output = false;
        let deadline = time::Instant::now() + Duration::from_secs(3);
        while time::Instant::now() < deadline {
            let Ok(Ok(Some(frame))) =
                time::timeout(Duration::from_millis(500), Codec::read_frame(&mut cr)).await
            else {
                break;
            };
            let msg: ServerMsg = postcard::from_bytes(&frame).unwrap();
            match msg {
                ServerMsg::Attached { .. } => saw_attached = true,
                ServerMsg::Output(_) => saw_output = true,
                ServerMsg::Error(e) => panic!("got error: {e:?}"),
                _ => {}
            }
            if saw_attached && saw_output {
                break;
            }
        }
        assert!(saw_attached, "missing Attached");
        assert!(saw_output, "missing Output");

        server.abort();
    }

    // `Ctrl+a : switch b <Enter>` moves this client from session "a" to the
    // pre-existing live session "b": registered on b, deregistered from a.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn command_prompt_switch_moves_client_between_sessions() {
        let _g = isolate();
        use std::time::{Duration, Instant};

        use tokio::io::{AsyncReadExt, split};

        let registry = Arc::new(crate::SessionRegistry::new());
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        let size = PtySize {
            rows: 8,
            cols: 24,
            pixel_width: 0,
            pixel_height: 0,
        };
        let cat = || SpawnSpec {
            program: "/bin/cat".into(),
            args: vec![],
            env: vec![],
            cwd: None,
        };

        // Pre-create the switch target "b" (0 clients, so it stays alive detached).
        registry
            .attach_or_create("b".into(), cat(), size, Arc::clone(&cfg))
            .await
            .unwrap();

        let (server_side, client_side) = duplex(64 * 1024);
        let reg = Arc::clone(&registry);
        let cfg2 = Arc::clone(&cfg);
        let server = tokio::spawn(async move { serve(server_side, 7, reg, cfg2).await });

        let (mut cr, mut cw) = split(client_side);
        let _ = client_handshake(&mut cr, &mut cw).await.unwrap();
        let attach = ClientMsg::AttachOrCreate {
            name: Some("a".into()),
            create_if_missing: true,
            cmd: Some(cat()),
            size,
        };
        Codec::write_frame(&mut cw, &postcard::to_allocvec(&attach).unwrap())
            .await
            .unwrap();

        // Drain server output so the socket never backs up.
        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            while cr.read(&mut buf).await.unwrap_or(0) > 0 {}
        });

        let clients_of = |entries: &[plexy_glass_protocol::SessionEntry], name: &str| {
            entries.iter().find(|e| e.name == name).map(|e| e.clients)
        };
        let wait_until = |want_a: u8, want_b: u8| {
            let registry = Arc::clone(&registry);
            async move {
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    let entries = registry.list().await;
                    if clients_of(&entries, "a") == Some(want_a)
                        && clients_of(&entries, "b") == Some(want_b)
                    {
                        return;
                    }
                    assert!(
                        Instant::now() <= deadline,
                        "timed out: a={:?} b={:?} (want a={want_a} b={want_b})",
                        clients_of(&entries, "a"),
                        clients_of(&entries, "b")
                    );
                    time::sleep(Duration::from_millis(20)).await;
                }
            }
        };

        // Attach completed: a has the client, b has none.
        wait_until(1, 0).await;

        // Drive the command prompt: Ctrl+a (0x01), ':', "switch b", Enter (0x0d).
        let input = ClientMsg::Input(bytes::Bytes::from_static(b"\x01:switch b\r"));
        Codec::write_frame(&mut cw, &postcard::to_allocvec(&input).unwrap())
            .await
            .unwrap();

        // The client has moved: b now has it, a has none.
        wait_until(0, 1).await;

        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn command_prompt_switch_auto_creates_declared_session() {
        // `:switch dev` when "dev" is declared but NOT running builds it from
        // the template (sourcing config from the live session's snapshot), then
        // switches. No boot loop here, so "dev" is not live until the switch.
        let _g = isolate();
        use std::time::{Duration, Instant};

        use tokio::io::{AsyncReadExt, split};

        let registry = Arc::new(crate::SessionRegistry::new());
        // Config declares "dev" (a 2-pane split) but it is never pre-built.
        let cfg = Arc::new(
            plexy_glass_config::parse_config(
                r#"session "dev" { window "w" { split vertical { pane; pane } } }"#,
            )
            .unwrap(),
        );
        let size = PtySize {
            rows: 8,
            cols: 24,
            pixel_width: 0,
            pixel_height: 0,
        };
        let cat = || SpawnSpec {
            program: "/bin/cat".into(),
            args: vec![],
            env: vec![],
            cwd: None,
        };

        // "dev" is not live yet.
        assert!(
            registry.get("dev").await.is_none(),
            "precondition: dev not built"
        );

        let (server_side, client_side) = duplex(64 * 1024);
        let reg = Arc::clone(&registry);
        let cfg2 = Arc::clone(&cfg);
        let server = tokio::spawn(async move { serve(server_side, 7, reg, cfg2).await });

        let (mut cr, mut cw) = split(client_side);
        let _ = client_handshake(&mut cr, &mut cw).await.unwrap();
        let attach = ClientMsg::AttachOrCreate {
            name: Some("a".into()),
            create_if_missing: true,
            cmd: Some(cat()),
            size,
        };
        Codec::write_frame(&mut cw, &postcard::to_allocvec(&attach).unwrap())
            .await
            .unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            while cr.read(&mut buf).await.unwrap_or(0) > 0 {}
        });

        let wait_dev_has_client = || {
            let registry = Arc::clone(&registry);
            async move {
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    let entries = registry.list().await;
                    if let Some(dev) = entries.iter().find(|e| e.name == "dev")
                        && dev.clients == 1
                    {
                        // dev was auto-built as the 2-pane template and the
                        // client moved onto it.
                        assert_eq!(dev.panes, 2, "auto-built from the 2-pane template");
                        return;
                    }
                    assert!(
                        Instant::now() <= deadline,
                        "dev never auto-created + switched: {entries:?}"
                    );
                    time::sleep(Duration::from_millis(20)).await;
                }
            }
        };

        // Give the attach a moment, then switch to the declared-not-running name.
        time::sleep(Duration::from_millis(100)).await;
        let input = ClientMsg::Input(bytes::Bytes::from_static(b"\x01:switch dev\r"));
        Codec::write_frame(&mut cw, &postcard::to_allocvec(&input).unwrap())
            .await
            .unwrap();

        wait_dev_has_client().await;
        server.abort();
    }

    // Pressing the prefix (Ctrl+a) through serve's input path arms the
    // session-level any_prefix_armed aggregate (which feeds the
    // `prefix-indicator` widget); an unbound follow-up key (Cancel) disarms it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prefix_press_arms_session_follow_up_key_disarms() {
        let _g = isolate();
        use std::time::{Duration, Instant};

        use tokio::io::{AsyncReadExt, split};

        let registry = Arc::new(crate::SessionRegistry::new());
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        let size = PtySize {
            rows: 8,
            cols: 24,
            pixel_width: 0,
            pixel_height: 0,
        };
        let cat = SpawnSpec {
            program: "/bin/cat".into(),
            args: vec![],
            env: vec![],
            cwd: None,
        };

        let (server_side, client_side) = duplex(64 * 1024);
        let reg = Arc::clone(&registry);
        let cfg2 = Arc::clone(&cfg);
        let server = tokio::spawn(async move { serve(server_side, 7, reg, cfg2).await });

        let (mut cr, mut cw) = split(client_side);
        let _ = client_handshake(&mut cr, &mut cw).await.unwrap();
        let attach = ClientMsg::AttachOrCreate {
            name: Some("prefixarm".into()),
            create_if_missing: true,
            cmd: Some(cat),
            size,
        };
        Codec::write_frame(&mut cw, &postcard::to_allocvec(&attach).unwrap())
            .await
            .unwrap();

        // Drain server output so the socket never backs up.
        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            while cr.read(&mut buf).await.unwrap_or(0) > 0 {}
        });

        // Wait for the client to be registered on the session.
        let session = {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if let Some(s) = registry.get("prefixarm").await
                    && s.clients.lock().await.len() == 1
                {
                    break s;
                }
                assert!(Instant::now() < deadline, "timed out waiting for attach");
                time::sleep(Duration::from_millis(20)).await;
            }
        };
        assert!(!session.any_prefix_armed().await, "armed before any input");

        let wait_armed = |want: bool, what: &'static str| {
            let session = Arc::clone(&session);
            async move {
                let deadline = Instant::now() + Duration::from_secs(5);
                while session.any_prefix_armed().await != want {
                    assert!(Instant::now() < deadline, "timed out waiting for {what}");
                    time::sleep(Duration::from_millis(10)).await;
                }
            }
        };

        // Ctrl+a (0x01) arms the prefix.
        let input = ClientMsg::Input(bytes::Bytes::from_static(b"\x01"));
        Codec::write_frame(&mut cw, &postcard::to_allocvec(&input).unwrap())
            .await
            .unwrap();
        wait_armed(true, "arm").await;

        // `e` is unbound after the prefix → Cancel → disarmed. (The prefix
        // waits indefinitely, no chord timeout, so only this key resolves it.)
        let input = ClientMsg::Input(bytes::Bytes::from_static(b"e"));
        Codec::write_frame(&mut cw, &postcard::to_allocvec(&input).unwrap())
            .await
            .unwrap();
        wait_armed(false, "disarm").await;

        server.abort();
    }

    // A real (v12) client handshake means `Ctrl+a w` delegates to the client
    // picker rather than opening the daemon overlay (see
    // `choose_session_version_gates_daemon_overlay_vs_client_picker` for that
    // gate in isolation); the client commits its choice by sending
    // `ClientMsg::SwitchSession` back over the wire, the same-daemon fast path.
    // Exercises that end-to-end: attach → delegate → `SwitchSession` → switch.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_picker_delegate_then_switch_session_switches() {
        let _g = isolate();
        use std::time::{Duration, Instant};

        use tokio::io::{AsyncReadExt, split};

        let registry = Arc::new(crate::SessionRegistry::new());
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        let size = PtySize {
            rows: 10,
            cols: 40,
            pixel_width: 0,
            pixel_height: 0,
        };
        let cat = || SpawnSpec {
            program: "/bin/cat".into(),
            args: vec![],
            env: vec![],
            cwd: None,
        };
        registry
            .attach_or_create("beta".into(), cat(), size, Arc::clone(&cfg))
            .await
            .unwrap();

        let (server_side, client_side) = duplex(64 * 1024);
        let reg = Arc::clone(&registry);
        let cfg2 = Arc::clone(&cfg);
        let server = tokio::spawn(async move { serve(server_side, 7, reg, cfg2).await });

        let (mut cr, mut cw) = split(client_side);
        let _ = client_handshake(&mut cr, &mut cw).await.unwrap();
        let attach = ClientMsg::AttachOrCreate {
            name: Some("alpha".into()),
            create_if_missing: true,
            cmd: Some(cat()),
            size,
        };
        Codec::write_frame(&mut cw, &postcard::to_allocvec(&attach).unwrap())
            .await
            .unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            while cr.read(&mut buf).await.unwrap_or(0) > 0 {}
        });

        let clients_of = |entries: &[plexy_glass_protocol::SessionEntry], name: &str| {
            entries.iter().find(|e| e.name == name).map(|e| e.clients)
        };
        let wait_until = |want_alpha: u8, want_beta: u8| {
            let registry = Arc::clone(&registry);
            async move {
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    let entries = registry.list().await;
                    if clients_of(&entries, "alpha") == Some(want_alpha)
                        && clients_of(&entries, "beta") == Some(want_beta)
                    {
                        return;
                    }
                    assert!(
                        Instant::now() <= deadline,
                        "timed out: alpha={:?} beta={:?}",
                        clients_of(&entries, "alpha"),
                        clients_of(&entries, "beta")
                    );
                    time::sleep(Duration::from_millis(20)).await;
                }
            }
        };
        wait_until(1, 0).await;

        // Ctrl+a w (0x01 'w'): on this v12 client, delegates to the client
        // picker instead of opening the daemon overlay (drained by the reader
        // task above; the delegation itself is covered in isolation by
        // `choose_session_version_gates_daemon_overlay_vs_client_picker`).
        let input = ClientMsg::Input(bytes::Bytes::from_static(b"\x01w"));
        Codec::write_frame(&mut cw, &postcard::to_allocvec(&input).unwrap())
            .await
            .unwrap();

        // The client renders its own picker and commits the choice by sending
        // `SwitchSession` back — the same-daemon fast path this task wires up.
        let switch = ClientMsg::SwitchSession {
            name: "beta".into(),
        };
        Codec::write_frame(&mut cw, &postcard::to_allocvec(&switch).unwrap())
            .await
            .unwrap();

        wait_until(0, 1).await;
        server.abort();
    }

    // `ConnVerb::ChooseSession` is version-gated: a v11 `ClientCtx` still opens
    // the daemon overlay (nothing on `inject_tx`); a v12+ `ClientCtx` sends
    // `OpenSessionPicker` on `inject_tx` and does NOT open the daemon overlay
    // (the client renders its own).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn choose_session_version_gates_daemon_overlay_vs_client_picker() {
        let _g = isolate();
        let registry = Arc::new(crate::SessionRegistry::new());
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        let (km, _skips) = plexy_glass_keys::build_keymap_with_skips(&cfg.keymap);
        let mut keymap = km;

        // v11: falls back to the daemon overlay.
        {
            let mut session = registry
                .attach_or_create("v11".into(), script_cat(), script_size(), Arc::clone(&cfg))
                .await
                .unwrap();
            let (switch_tx, _switch_rx) = mpsc::unbounded_channel();
            let (inject_tx, mut inject_rx) = mpsc::unbounded_channel();
            let mut client_id = 0u64;
            let prefix_armed = Arc::new(AtomicBool::new(false));
            let mut ctx = ClientCtx {
                session: &mut session,
                client_id: &mut client_id,
                size: script_size(),
                registry: &registry,
                switch_tx: &switch_tx,
                prefix_armed: &prefix_armed,
                remote: false,
                version: 11,
                inject_tx: &inject_tx,
            };
            run_connection_verb(&mut ctx, &mut keymap, ConnVerb::ChooseSession).await;
            let overlay_open = {
                let m = session.window_manager.lock().await;
                m.overlay().is_some()
            };
            assert!(overlay_open, "v11 client still gets the daemon overlay");
            assert!(
                inject_rx.try_recv().is_err(),
                "v11 client gets nothing on inject_tx"
            );
            session.terminate_panes().await;
        }

        // v12: delegates to the client, no daemon overlay.
        {
            let mut session = registry
                .attach_or_create("v12".into(), script_cat(), script_size(), Arc::clone(&cfg))
                .await
                .unwrap();
            let (switch_tx, _switch_rx) = mpsc::unbounded_channel();
            let (inject_tx, mut inject_rx) = mpsc::unbounded_channel();
            let mut client_id = 0u64;
            let prefix_armed = Arc::new(AtomicBool::new(false));
            let mut ctx = ClientCtx {
                session: &mut session,
                client_id: &mut client_id,
                size: script_size(),
                registry: &registry,
                switch_tx: &switch_tx,
                prefix_armed: &prefix_armed,
                remote: false,
                version: 12,
                inject_tx: &inject_tx,
            };
            run_connection_verb(&mut ctx, &mut keymap, ConnVerb::ChooseSession).await;
            let overlay_open = {
                let m = session.window_manager.lock().await;
                m.overlay().is_some()
            };
            assert!(!overlay_open, "v12 client does not get the daemon overlay");
            match inject_rx.try_recv() {
                Ok(RenderInject::Msg(ServerMsg::OpenSessionPicker { sessions, current })) => {
                    assert_eq!(current, "v12");
                    assert!(sessions.iter().any(|e| e.name == "v12"));
                }
                other => panic!("expected OpenSessionPicker on inject_tx, got {other:?}"),
            }
            session.terminate_panes().await;
        }
    }

    // `Ctrl+a W` opens the choose-tree; the tree lists every session
    // (sorted: alpha then beta, each session+window+pane = 3 rows). Three `j`
    // moves the selection to the "beta" session node; Enter switches there.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn choose_tree_switches_sessions() {
        let _g = isolate();
        use std::time::{Duration, Instant};

        use tokio::io::{AsyncReadExt, split};

        let registry = Arc::new(crate::SessionRegistry::new());
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        let size = PtySize {
            rows: 16,
            cols: 60,
            pixel_width: 0,
            pixel_height: 0,
        };
        let cat = || SpawnSpec {
            program: "/bin/cat".into(),
            args: vec![],
            env: vec![],
            cwd: None,
        };
        registry
            .attach_or_create("beta".into(), cat(), size, Arc::clone(&cfg))
            .await
            .unwrap();

        let (server_side, client_side) = duplex(64 * 1024);
        let reg = Arc::clone(&registry);
        let cfg2 = Arc::clone(&cfg);
        let server = tokio::spawn(async move { serve(server_side, 7, reg, cfg2).await });

        let (mut cr, mut cw) = split(client_side);
        let _ = client_handshake(&mut cr, &mut cw).await.unwrap();
        let attach = ClientMsg::AttachOrCreate {
            name: Some("alpha".into()),
            create_if_missing: true,
            cmd: Some(cat()),
            size,
        };
        Codec::write_frame(&mut cw, &postcard::to_allocvec(&attach).unwrap())
            .await
            .unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            while cr.read(&mut buf).await.unwrap_or(0) > 0 {}
        });

        let clients_of = |entries: &[plexy_glass_protocol::SessionEntry], name: &str| {
            entries.iter().find(|e| e.name == name).map(|e| e.clients)
        };
        let wait_until = |want_alpha: u8, want_beta: u8| {
            let registry = Arc::clone(&registry);
            async move {
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    let entries = registry.list().await;
                    if clients_of(&entries, "alpha") == Some(want_alpha)
                        && clients_of(&entries, "beta") == Some(want_beta)
                    {
                        return;
                    }
                    assert!(
                        Instant::now() <= deadline,
                        "timed out: alpha={:?} beta={:?}",
                        clients_of(&entries, "alpha"),
                        clients_of(&entries, "beta")
                    );
                    time::sleep(Duration::from_millis(20)).await;
                }
            }
        };
        wait_until(1, 0).await;

        // Ctrl+a W opens the tree; jjj selects the "beta" session node; Enter switches.
        let input = ClientMsg::Input(bytes::Bytes::from_static(b"\x01Wjjj\r"));
        Codec::write_frame(&mut cw, &postcard::to_allocvec(&input).unwrap())
            .await
            .unwrap();

        wait_until(0, 1).await;
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn history_jump_lands_in_block_mode_on_the_target_block() {
        use plexy_glass_emulator::{Row, RowMark};
        let _g = isolate();
        let registry = Arc::new(crate::SessionRegistry::new());
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        let size = PtySize {
            rows: 16,
            cols: 60,
            pixel_width: 0,
            pixel_height: 0,
        };
        let mut session = registry
            .attach_or_create("h".into(), script_cat(), size, Arc::clone(&cfg))
            .await
            .unwrap();
        let prompt_row = |cmd: &str, cols: u16| {
            let mut r = Row::blank(cols);
            for (i, ch) in format!("$ {cmd}").chars().enumerate() {
                if (i as u16) < cols {
                    r.cells[i].grapheme = ch.to_string().into();
                }
            }
            r.mark.set(RowMark::PROMPT_START);
            r.mark.set_prompt_end(2);
            r
        };
        let (target_window, target_pane) = {
            let m = session.window_manager.lock().await;
            let pid = m.active_window().active();
            m.active_window().pane(pid).unwrap().with_screen_mut(|scr| {
                let cols = scr.active.cols;
                scr.active.rows[0] = prompt_row("ls", cols);
                scr.active.rows[1] = {
                    let mut r = Row::blank(cols);
                    r.cells[0].grapheme = "a".into();
                    r.mark.set(RowMark::OUTPUT_START);
                    r
                };
                scr.active.rows[2] = prompt_row("pwd", cols);
            });
            (m.active_window().id, pid)
        };

        let (switch_tx, _switch_rx) = mpsc::unbounded_channel();
        let (inject_tx, _inject_rx) = mpsc::unbounded_channel();
        let mut client_id = 0u64;
        let prefix_armed = Arc::new(AtomicBool::new(false));
        let mut ctx = ClientCtx {
            session: &mut session,
            client_id: &mut client_id,
            size,
            registry: &registry,
            switch_tx: &switch_tx,
            prefix_armed: &prefix_armed,
            remote: false,
            version: 12,
            inject_tx: &inject_tx,
        };
        // Jump to the "ls" block (prompt line 0) in the same session.
        ctx.dispatch_history_jump(plexy_glass_mux::HistoryTarget {
            session: "h".into(),
            window: target_window,
            pane: target_pane,
            prompt_line: 0,
            command: "ls".into(),
        })
        .await;

        let m = session.window_manager.lock().await;
        let pane = m.active_window().pane(target_pane).unwrap();
        assert!(pane.is_in_block_mode(), "jump landed in block mode");
        drop(m);
        session.terminate_panes().await;
    }

    #[test]
    fn build_history_entries_orders_current_pane_first_newest_first() {
        use plexy_glass_mux::{PaneId, WindowId};

        use crate::session::{HistoryBlock, SessionHistory};
        let blk = |pane: u32, line: u32, cmd: &str| HistoryBlock {
            window: WindowId(0),
            window_idx: 0,
            pane: PaneId(pane),
            prompt_line: line,
            command: cmd.into(),
            exit: Some(0),
            duration: None,
            haystack: cmd.to_lowercase(),
        };
        let snaps = vec![
            SessionHistory {
                name: "cur".into(),
                // pane 1 (current) has two blocks newest-first; pane 0 one block.
                blocks: vec![
                    blk(1, 9, "cur-p1-new"),
                    blk(1, 3, "cur-p1-old"),
                    blk(0, 5, "cur-p0"),
                ],
            },
            SessionHistory {
                name: "other".into(),
                blocks: vec![blk(0, 7, "other")],
            },
        ];
        let entries = build_history_entries(&snaps, "cur", Some(PaneId(1)));
        // Current pane (1) first, newest-first; then current session's pane 0;
        // then the other session.
        let cmds: Vec<&str> = entries.iter().map(|e| e.command.as_str()).collect();
        assert_eq!(cmds, vec!["cur-p1-new", "cur-p1-old", "cur-p0", "other"]);
        assert!(entries.len() <= HISTORY_ENTRY_CAP);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn history_overlay_open_and_enter_jumps_to_newest_block() {
        use plexy_glass_emulator::{Row, RowMark};
        let _g = isolate();
        let registry = Arc::new(crate::SessionRegistry::new());
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        let session = registry
            .attach_or_create("h".into(), script_cat(), script_size(), Arc::clone(&cfg))
            .await
            .unwrap();
        // Build a prompt row "$ <cmd>" with the command starting at col 2.
        let prompt_row = |cmd: &str, cols: u16| {
            let mut r = Row::blank(cols);
            for (i, ch) in format!("$ {cmd}").chars().enumerate() {
                if (i as u16) < cols {
                    r.cells[i].grapheme = ch.to_string().into();
                }
            }
            r.mark.set(RowMark::PROMPT_START);
            r.mark.set_prompt_end(2);
            r
        };
        {
            let m = session.window_manager.lock().await;
            let pid = m.active_window().active();
            m.active_window().pane(pid).unwrap().with_screen_mut(|scr| {
                let cols = scr.active.cols;
                scr.active.rows[0] = prompt_row("ls", cols);
                scr.active.rows[1] = {
                    let mut r = Row::blank(cols);
                    r.cells[0].grapheme = "a".into();
                    r.mark.set(RowMark::OUTPUT_START);
                    r
                };
                scr.active.rows[2] = prompt_row("pwd", cols);
                scr.active.rows[3] = {
                    let mut r = Row::blank(cols);
                    r.cells[0].grapheme = "b".into();
                    r.mark.set(RowMark::OUTPUT_START);
                    r
                };
            });
        }
        open_history_overlay(&session, &registry).await;
        // Selection defaults to the newest current-pane block ("pwd"); Enter jumps.
        let result = {
            let mut m = session.window_manager.lock().await;
            m.handle_overlay_key(&plexy_glass_mux::KeyEvent::plain(
                plexy_glass_mux::Key::Enter,
            ))
        };
        match result {
            OverlayKeyResult::History(t) => {
                assert_eq!(t.session, "h");
                assert_eq!(
                    t.command, "pwd",
                    "newest current-pane block selected by default"
                );
            }
            other => panic!("expected History jump, got {other:?}"),
        }
        session.terminate_panes().await;
    }

    /// `hints` is interactive-only (opens an overlay over the active pane).
    /// Headless `cmd "hints"` must be refused with the standard message.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn hints_verb_refused_headless() {
        let _g = isolate();
        let registry = Arc::new(crate::SessionRegistry::new());
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        let session = registry
            .attach_or_create("h".into(), script_cat(), script_size(), Arc::clone(&cfg))
            .await
            .unwrap();
        let (ok, message) = run_prompt_line(&session, &registry, "hints").await;
        assert!(!ok, "hints must be refused headless");
        assert_eq!(
            message.unwrap_or_default(),
            "hints: requires an attached client"
        );
        session.terminate_panes().await;
    }

    /// When the pane's grid contains no recognisable hint targets, `open_hints_overlay`
    /// must flash "no hint targets" on the status line and leave no overlay open.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn open_hints_overlay_flashes_no_targets_on_empty_grid() {
        let _g = isolate();
        let registry = Arc::new(crate::SessionRegistry::new());
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        let session = registry
            .attach_or_create("h2".into(), script_cat(), script_size(), Arc::clone(&cfg))
            .await
            .unwrap();
        // The freshly-spawned pane's grid is blank, no URLs or hint targets.
        open_hints_overlay(&session).await;
        // Overlay must NOT have been opened.
        let overlay = {
            let m = session.window_manager.lock().await;
            m.overlay().is_some()
        };
        assert!(!overlay, "no overlay on empty grid");
        // Status message must have been set.
        let msg = {
            let mut m = session.window_manager.lock().await;
            m.take_active_message().map(str::to_string)
        };
        assert_eq!(msg.as_deref(), Some("no hint targets"));
        session.terminate_panes().await;
    }

    /// `open_hints_overlay` opens the hint overlay when the grid contains a URL.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn open_hints_overlay_opens_overlay_when_targets_found() {
        let _g = isolate();
        let registry = Arc::new(crate::SessionRegistry::new());
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        let session = registry
            .attach_or_create("h3".into(), script_cat(), script_size(), Arc::clone(&cfg))
            .await
            .unwrap();
        // Manually write a URL into the active pane's grid.
        {
            let m = session.window_manager.lock().await;
            let pid = m.active_window().active();
            m.active_window().pane(pid).unwrap().with_screen_mut(|scr| {
                let url = "https://example.com";
                for (i, ch) in url.chars().enumerate() {
                    if (i as u16) < scr.active.cols {
                        scr.active.rows[0].cells[i].grapheme = ch.to_string().into();
                    }
                }
            });
        }
        open_hints_overlay(&session).await;
        // Overlay must be open.
        let overlay = {
            let m = session.window_manager.lock().await;
            m.overlay().is_some()
        };
        assert!(overlay, "overlay must be open when URL is present");
        session.terminate_panes().await;
    }

    /// When `hints.enabled = false`, `open_hints_overlay` must be a no-op
    /// even when the pane's grid contains a URL.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn open_hints_overlay_no_op_when_disabled() {
        let _g = isolate();
        let registry = Arc::new(crate::SessionRegistry::new());
        let mut cfg = plexy_glass_config::built_in_default();
        cfg.hints.enabled = false;
        let cfg = Arc::new(cfg);
        let session = registry
            .attach_or_create("h4".into(), script_cat(), script_size(), Arc::clone(&cfg))
            .await
            .unwrap();
        // Write a URL into the active pane's grid.
        {
            let m = session.window_manager.lock().await;
            let pid = m.active_window().active();
            m.active_window().pane(pid).unwrap().with_screen_mut(|scr| {
                let url = "https://example.com";
                for (i, ch) in url.chars().enumerate() {
                    if (i as u16) < scr.active.cols {
                        scr.active.rows[0].cells[i].grapheme = ch.to_string().into();
                    }
                }
            });
        }
        open_hints_overlay(&session).await;
        // Overlay must NOT have been opened, `enabled=false` is a no-op.
        let overlay = {
            let m = session.window_manager.lock().await;
            m.overlay().is_some()
        };
        assert!(!overlay, "overlay must not open when hints.enabled = false");
        session.terminate_panes().await;
    }

    #[test]
    fn build_palette_entries_resolves_bound_keys() {
        let cfg = plexy_glass_config::built_in_default();
        let entries = build_palette_entries(&cfg);
        let zoom = entries.iter().find(|e| e.label == "Zoom pane").unwrap();
        // zoom_toggle is bound to `prefix z`; the resolved prefix is C-a by default.
        assert!(
            zoom.key.as_deref().unwrap_or("").contains('z'),
            "{:?}",
            zoom.key
        );
        // Layout presets have no default binding of their own (only the
        // next_layout cycle chord is bound), so they show no key.
        let np = entries.iter().find(|e| e.label == "Layout: tiled").unwrap();
        assert_eq!(
            np.key, None,
            "unbound-by-default layout preset shows no key"
        );
    }

    /// `palette` is interactive-only (opens an overlay over the active pane).
    /// Headless `cmd "palette"` must be refused with the standard message.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn palette_verb_refused_headless() {
        let _g = isolate();
        let registry = Arc::new(crate::SessionRegistry::new());
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        let session = registry
            .attach_or_create("p".into(), script_cat(), script_size(), Arc::clone(&cfg))
            .await
            .unwrap();
        let (ok, message) = run_prompt_line(&session, &registry, "palette").await;
        assert!(!ok, "palette must be refused headless");
        assert_eq!(
            message.unwrap_or_default(),
            "palette: requires an attached client"
        );
        session.terminate_panes().await;
    }

    /// `open_palette_overlay` opens the palette with keys resolved from the
    /// active keymap.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn open_palette_overlay_opens_overlay() {
        let _g = isolate();
        let registry = Arc::new(crate::SessionRegistry::new());
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        let session = registry
            .attach_or_create("p2".into(), script_cat(), script_size(), Arc::clone(&cfg))
            .await
            .unwrap();
        open_palette_overlay(&session).await;
        let overlay = {
            let m = session.window_manager.lock().await;
            m.overlay().is_some()
        };
        assert!(overlay, "palette overlay must be open");
        session.terminate_panes().await;
    }

    #[test]
    fn build_tree_nodes_marks_only_current_path() {
        use plexy_glass_mux::{PaneId, WindowId};

        use crate::session::{SessionTree, WindowTree};
        let snaps = vec![
            SessionTree {
                name: "cur".into(),
                active_window: 1,
                total_panes: 3,
                windows: vec![
                    WindowTree {
                        id: WindowId(0),
                        name: "w0".into(),
                        active_pane: PaneId(0),
                        panes: vec![(PaneId(0), None)],
                    },
                    WindowTree {
                        id: WindowId(1),
                        name: "w1".into(),
                        active_pane: PaneId(2),
                        panes: vec![(PaneId(1), None), (PaneId(2), Some("p".into()))],
                    },
                ],
            },
            SessionTree {
                name: "other".into(),
                active_window: 0,
                total_panes: 1,
                windows: vec![WindowTree {
                    id: WindowId(0),
                    name: "w0".into(),
                    active_pane: PaneId(0),
                    panes: vec![(PaneId(0), None)],
                }],
            },
        ];
        let nodes = build_tree_nodes(&snaps, "cur");
        let find = |pred: &dyn Fn(&plexy_glass_mux::TreeNode) -> bool| {
            nodes.iter().find(|n| pred(n)).expect("node present")
        };
        // Current session node + its active window (index 1 → WindowId(1)) + that
        // window's active pane (PaneId(2)) are the only marked nodes in "cur".
        assert!(find(&|n| n.session == "cur" && n.window.is_none()).is_current);
        assert!(
            find(&|n| n.session == "cur" && n.window == Some(WindowId(1)) && n.pane.is_none())
                .is_current
        );
        assert!(
            !find(&|n| n.session == "cur" && n.window == Some(WindowId(0)) && n.pane.is_none())
                .is_current
        );
        assert!(find(&|n| n.pane == Some(PaneId(2))).is_current);
        assert!(!find(&|n| n.session == "cur" && n.pane == Some(PaneId(1))).is_current);
        // Label formats.
        assert_eq!(find(&|n| n.pane == Some(PaneId(2))).label, "pane 2: p");
        assert_eq!(
            find(&|n| n.session == "cur" && n.window == Some(WindowId(1)) && n.pane.is_none())
                .label,
            "2: w1"
        );
        // The other (non-current) session's whole subtree is unmarked.
        assert!(
            nodes
                .iter()
                .filter(|n| n.session == "other")
                .all(|n| !n.is_current)
        );
    }

    // `Ctrl+a W` then navigate to the *beta* window node and rename it, a
    // cross-session rename (the client is attached to alpha). Asserts the rename
    // landed on beta's `WindowManager` via a fresh `tree_snapshot` of beta.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn choose_tree_renames_window_in_other_session() {
        let _g = isolate();
        use std::time::{Duration, Instant};

        use tokio::io::{AsyncReadExt, split};

        let registry = Arc::new(crate::SessionRegistry::new());
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        let size = PtySize {
            rows: 16,
            cols: 60,
            pixel_width: 0,
            pixel_height: 0,
        };
        let cat = || SpawnSpec {
            program: "/bin/cat".into(),
            args: vec![],
            env: vec![],
            cwd: None,
        };
        registry
            .attach_or_create("beta".into(), cat(), size, Arc::clone(&cfg))
            .await
            .unwrap();

        let (server_side, client_side) = duplex(64 * 1024);
        let reg = Arc::clone(&registry);
        let cfg2 = Arc::clone(&cfg);
        let server = tokio::spawn(async move { serve(server_side, 7, reg, cfg2).await });

        let (mut cr, mut cw) = split(client_side);
        let _ = client_handshake(&mut cr, &mut cw).await.unwrap();
        let attach = ClientMsg::AttachOrCreate {
            name: Some("alpha".into()),
            create_if_missing: true,
            cmd: Some(cat()),
            size,
        };
        Codec::write_frame(&mut cw, &postcard::to_allocvec(&attach).unwrap())
            .await
            .unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            while cr.read(&mut buf).await.unwrap_or(0) > 0 {}
        });

        // Wait for the attach to register on alpha.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let entries = registry.list().await;
            if entries
                .iter()
                .find(|e| e.name == "alpha")
                .map(|e| e.clients)
                == Some(1)
            {
                break;
            }
            assert!(
                Instant::now() <= deadline,
                "alpha never registered a client"
            );
            time::sleep(Duration::from_millis(20)).await;
        }

        // Tree nodes (sorted): alpha(0), alpha-win(1), alpha-pane(2), beta(3),
        // beta-win(4), .. so 4 `j` lands on beta's window node. `r` seeds the
        // rename buffer with "shell"; "ed" + Enter commits "shelled".
        let input = ClientMsg::Input(bytes::Bytes::from_static(b"\x01Wjjjjred\r"));
        Codec::write_frame(&mut cw, &postcard::to_allocvec(&input).unwrap())
            .await
            .unwrap();

        // Beta's first window name must become "shelled".
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(b) = registry.get("beta").await {
                let st = b.tree_snapshot().await;
                if st.windows[0].name == "shelled" {
                    break;
                }
            }
            if Instant::now() > deadline {
                let name = match registry.get("beta").await {
                    Some(b) => b.tree_snapshot().await.windows[0].name.clone(),
                    None => "<gone>".into(),
                };
                panic!("beta window not renamed; got {name:?}");
            }
            time::sleep(Duration::from_millis(20)).await;
        }
        server.abort();
    }

    fn tree_key(c: char) -> plexy_glass_mux::KeyEvent {
        plexy_glass_mux::KeyEvent::plain(plexy_glass_mux::Key::Char(c))
    }

    // RenameSession success path: the registry re-keys, and the adapter
    // commits the rename into the STILL-OPEN tree (commit-on-success, the
    // model was not optimistically mutated): session row label/name,
    // descendants' `session` fields, and collapsed NodeKeys all re-stamped;
    // no status message.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tree_rename_session_commits_and_restamps_open_tree() {
        let _g = isolate();
        use plexy_glass_mux::{NodeKey, Overlay, TreeAction, TreeKind, session_label};
        let registry = Arc::new(crate::SessionRegistry::new());
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        let size = PtySize {
            rows: 16,
            cols: 60,
            pixel_width: 0,
            pixel_height: 0,
        };
        let cat = || SpawnSpec {
            program: "/bin/cat".into(),
            args: vec![],
            env: vec![],
            cwd: None,
        };
        let mut session = registry
            .attach_or_create("alpha".into(), cat(), size, Arc::clone(&cfg))
            .await
            .unwrap();
        registry
            .attach_or_create("beta".into(), cat(), size, Arc::clone(&cfg))
            .await
            .unwrap();
        open_tree_overlay(&session, &registry).await;
        {
            // Rows (sorted): alpha(0) alpha-win(1) alpha-pane(2) beta(3)
            // beta-win(4) beta-pane(5). Collapse beta's window AND beta itself
            // so the rename must re-key both NodeKey shapes.
            let mut m = session.window_manager.lock().await;
            for _ in 0..4 {
                m.handle_overlay_key(&tree_key('j'));
            }
            m.handle_overlay_key(&tree_key('h')); // collapse beta's window
            m.handle_overlay_key(&tree_key('k'));
            m.handle_overlay_key(&tree_key('h')); // collapse beta
        }

        let (switch_tx, _switch_rx) = mpsc::unbounded_channel();
        let (inject_tx, _inject_rx) = mpsc::unbounded_channel();
        let mut client_id = 0u64;
        let prefix_armed = Arc::new(AtomicBool::new(false));
        let mut ctx = ClientCtx {
            session: &mut session,
            client_id: &mut client_id,
            size,
            registry: &registry,
            switch_tx: &switch_tx,
            prefix_armed: &prefix_armed,
            remote: false,
            version: 12,
            inject_tx: &inject_tx,
        };
        ctx.dispatch_tree_action(TreeAction::RenameSession {
            old: "beta".into(),
            new: "gamma".into(),
        })
        .await;

        assert!(
            registry.get("beta").await.is_none(),
            "registry re-keyed away from old"
        );
        assert!(
            registry.get("gamma").await.is_some(),
            "registry resolves the new name"
        );

        let mut m = session.window_manager.lock().await;
        assert_eq!(
            m.take_active_message(),
            None,
            "success sets no status message"
        );
        let Some(Overlay::Tree(state)) = m.overlay() else {
            panic!("tree overlay must still be open after a rename");
        };
        let row = state
            .nodes
            .iter()
            .find(|n| n.kind() == TreeKind::Session && n.session == "gamma")
            .expect("renamed session row present");
        assert_eq!(row.name, "gamma");
        assert_eq!(row.label, session_label("gamma", 1, 1));
        assert!(
            state.nodes.iter().all(|n| n.session != "beta"),
            "no row may keep the old session identity"
        );
        assert_eq!(
            state.nodes.iter().filter(|n| n.session == "gamma").count(),
            3,
            "session + window + pane rows all re-stamped"
        );
        assert!(state.collapsed.contains(&NodeKey::Session("gamma".into())));
        assert!(
            state
                .collapsed
                .iter()
                .any(|k| matches!(k, NodeKey::Window { session, .. } if session == "gamma")),
            "window NodeKey re-keyed"
        );
        assert!(
            !state.collapsed.iter().any(|k| matches!(
                k,
                NodeKey::Session(s) if s == "beta"
            ) || matches!(
                k,
                NodeKey::Window { session, .. } if session == "beta"
            )),
            "no collapsed key may keep the old session identity"
        );
    }

    // RenameSession failure path (live-name collision): status message set,
    // tree untouched. Nothing was optimistically mutated, so nothing to revert.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tree_rename_session_collision_sets_status_and_leaves_tree() {
        let _g = isolate();
        use plexy_glass_mux::{Overlay, TreeAction};
        let registry = Arc::new(crate::SessionRegistry::new());
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        let size = PtySize {
            rows: 16,
            cols: 60,
            pixel_width: 0,
            pixel_height: 0,
        };
        let cat = || SpawnSpec {
            program: "/bin/cat".into(),
            args: vec![],
            env: vec![],
            cwd: None,
        };
        let mut session = registry
            .attach_or_create("alpha".into(), cat(), size, Arc::clone(&cfg))
            .await
            .unwrap();
        registry
            .attach_or_create("beta".into(), cat(), size, Arc::clone(&cfg))
            .await
            .unwrap();
        open_tree_overlay(&session, &registry).await;
        let nodes_before = {
            let m = session.window_manager.lock().await;
            let Some(Overlay::Tree(state)) = m.overlay() else {
                panic!("tree overlay must be open");
            };
            state.nodes.clone()
        };

        let (switch_tx, _switch_rx) = mpsc::unbounded_channel();
        let (inject_tx, _inject_rx) = mpsc::unbounded_channel();
        let mut client_id = 0u64;
        let prefix_armed = Arc::new(AtomicBool::new(false));
        let mut ctx = ClientCtx {
            session: &mut session,
            client_id: &mut client_id,
            size,
            registry: &registry,
            switch_tx: &switch_tx,
            prefix_armed: &prefix_armed,
            remote: false,
            version: 12,
            inject_tx: &inject_tx,
        };
        ctx.dispatch_tree_action(TreeAction::RenameSession {
            old: "beta".into(),
            new: "alpha".into(),
        })
        .await;

        assert!(registry.get("alpha").await.is_some());
        assert!(
            registry.get("beta").await.is_some(),
            "failed rename leaves the key alone"
        );
        let mut m = session.window_manager.lock().await;
        let msg = m
            .take_active_message()
            .expect("collision must set a status message");
        assert!(
            msg.contains("already exists"),
            "status message carries the registry error; got {msg:?}"
        );
        let Some(Overlay::Tree(state)) = m.overlay() else {
            panic!("tree overlay must still be open");
        };
        assert_eq!(
            state.nodes, nodes_before,
            "failed rename leaves the tree untouched"
        );
    }

    // RenameSession refusal path: renaming TO a config-declared session name
    // is refused outright (the template would shadow the renamed session's
    // saved state at next boot, silent data loss), with the exact status
    // message and the registry untouched.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tree_rename_session_to_declared_name_is_refused() {
        let _g = isolate();
        use plexy_glass_mux::TreeAction;
        let registry = Arc::new(crate::SessionRegistry::new());
        // "dev" is declared in config but not running, so the refusal must not
        // depend on a live collision.
        let cfg = Arc::new(
            plexy_glass_config::parse_config(r#"session "dev" { window "w" { pane } }"#)
                .expect("declared-session config"),
        );
        let size = PtySize {
            rows: 16,
            cols: 60,
            pixel_width: 0,
            pixel_height: 0,
        };
        let cat = || SpawnSpec {
            program: "/bin/cat".into(),
            args: vec![],
            env: vec![],
            cwd: None,
        };
        let mut session = registry
            .attach_or_create("alpha".into(), cat(), size, Arc::clone(&cfg))
            .await
            .unwrap();

        let (switch_tx, _switch_rx) = mpsc::unbounded_channel();
        let (inject_tx, _inject_rx) = mpsc::unbounded_channel();
        let mut client_id = 0u64;
        let prefix_armed = Arc::new(AtomicBool::new(false));
        let mut ctx = ClientCtx {
            session: &mut session,
            client_id: &mut client_id,
            size,
            registry: &registry,
            switch_tx: &switch_tx,
            prefix_armed: &prefix_armed,
            remote: false,
            version: 12,
            inject_tx: &inject_tx,
        };
        ctx.dispatch_tree_action(TreeAction::RenameSession {
            old: "alpha".into(),
            new: "dev".into(),
        })
        .await;

        assert!(
            registry.get("alpha").await.is_some(),
            "refused rename leaves the key alone"
        );
        assert!(
            registry.get("dev").await.is_none(),
            "no session appears under the declared name"
        );
        let mut m = session.window_manager.lock().await;
        let msg = m
            .take_active_message()
            .expect("refusal must set a status message");
        assert_eq!(msg, "'dev' is a declared session name — choose another");
    }

    // Drive break-pane and join-pane through the full key/verb path and assert
    // the window/pane structure via `tree_snapshot` (the screen-scrape e2e harness
    // has no count API). split → break grows to 2 windows; mark + select + join
    // moves the pane back into window 0 and removes the emptied source window.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn break_and_join_panes_via_keys() {
        let _g = isolate();
        use std::time::{Duration, Instant};

        use tokio::io::{AsyncReadExt, split};

        let registry = Arc::new(crate::SessionRegistry::new());
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        let size = PtySize {
            rows: 16,
            cols: 60,
            pixel_width: 0,
            pixel_height: 0,
        };
        let cat = || SpawnSpec {
            program: "/bin/cat".into(),
            args: vec![],
            env: vec![],
            cwd: None,
        };

        let (server_side, client_side) = duplex(64 * 1024);
        let reg = Arc::clone(&registry);
        let cfg2 = Arc::clone(&cfg);
        let server = tokio::spawn(async move { serve(server_side, 7, reg, cfg2).await });

        let (mut cr, mut cw) = split(client_side);
        let _ = client_handshake(&mut cr, &mut cw).await.unwrap();
        let attach = ClientMsg::AttachOrCreate {
            name: Some("main".into()),
            create_if_missing: true,
            cmd: Some(cat()),
            size,
        };
        Codec::write_frame(&mut cw, &postcard::to_allocvec(&attach).unwrap())
            .await
            .unwrap();
        tokio::spawn(async move {
            let mut b = [0u8; 4096];
            while cr.read(&mut b).await.unwrap_or(0) > 0 {}
        });

        let poll = |want_windows: usize, want_panes: usize| {
            let registry = Arc::clone(&registry);
            async move {
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    if let Some(s) = registry.get("main").await {
                        let st = s.tree_snapshot().await;
                        if st.windows.len() == want_windows && st.total_panes == want_panes {
                            return;
                        }
                    }
                    if Instant::now() > deadline {
                        let (w, p) = match registry.get("main").await {
                            Some(s) => {
                                let st = s.tree_snapshot().await;
                                (st.windows.len(), st.total_panes)
                            }
                            None => (0, 0),
                        };
                        panic!(
                            "timed out: windows={w} panes={p} (want w={want_windows} p={want_panes})"
                        );
                    }
                    time::sleep(Duration::from_millis(20)).await;
                }
            }
        };

        poll(1, 1).await; // attach settled
        // Ctrl+a v (split) then Ctrl+a ! (break) → 2 windows, 2 panes.
        Codec::write_frame(
            &mut cw,
            &postcard::to_allocvec(&ClientMsg::Input(bytes::Bytes::from_static(b"\x01v\x01!")))
                .unwrap(),
        )
        .await
        .unwrap();
        poll(2, 2).await;
        // Ctrl+a m (mark) · Ctrl+a 1 (select window 0) · :join-pane → 1 window, 2 panes.
        Codec::write_frame(
            &mut cw,
            &postcard::to_allocvec(&ClientMsg::Input(bytes::Bytes::from_static(
                b"\x01m\x011\x01:join-pane\r",
            )))
            .unwrap(),
        )
        .await
        .unwrap();
        poll(1, 2).await;

        // Identity, not just cardinality: the surviving window holds both panes
        // and the joined pane (PaneId 1, the one that was broken out) is active.
        let st = registry.get("main").await.unwrap().tree_snapshot().await;
        let ids: Vec<_> = st.windows[0].panes.iter().map(|(id, _)| *id).collect();
        assert!(
            ids.contains(&plexy_glass_mux::PaneId(0)) && ids.contains(&plexy_glass_mux::PaneId(1)),
            "both panes back in one window: {ids:?}"
        );
        assert_eq!(
            st.windows[0].active_pane,
            plexy_glass_mux::PaneId(1),
            "joined pane is active"
        );

        server.abort();
    }

    // Push a paste buffer, then `Ctrl+a ]`: the bytes reach the active pane
    // (a `/bin/cat` pane echoes them, so they appear on its screen). And
    // `Ctrl+a =` + `d` deletes a buffer (the registry count drops).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn paste_buffer_reaches_pane_and_chooser_deletes() {
        let _g = isolate();
        use std::time::{Duration, Instant};

        use tokio::io::{AsyncReadExt, split};

        let registry = Arc::new(crate::SessionRegistry::new());
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        let size = PtySize {
            rows: 16,
            cols: 60,
            pixel_width: 0,
            pixel_height: 0,
        };
        let cat = || SpawnSpec {
            program: "/bin/cat".into(),
            args: vec![],
            env: vec![],
            cwd: None,
        };

        let (server_side, client_side) = duplex(64 * 1024);
        let reg = Arc::clone(&registry);
        let cfg2 = Arc::clone(&cfg);
        let server = tokio::spawn(async move { serve(server_side, 7, reg, cfg2).await });

        let (mut cr, mut cw) = split(client_side);
        let _ = client_handshake(&mut cr, &mut cw).await.unwrap();
        let attach = ClientMsg::AttachOrCreate {
            name: Some("main".into()),
            create_if_missing: true,
            cmd: Some(cat()),
            size,
        };
        Codec::write_frame(&mut cw, &postcard::to_allocvec(&attach).unwrap())
            .await
            .unwrap();
        tokio::spawn(async move {
            let mut b = [0u8; 4096];
            while cr.read(&mut b).await.unwrap_or(0) > 0 {}
        });

        // Wait for the session to exist.
        let deadline = Instant::now() + Duration::from_secs(5);
        while registry.get("main").await.is_none() {
            assert!(Instant::now() <= deadline, "session never created");
            time::sleep(Duration::from_millis(20)).await;
        }

        // Inject a buffer (deterministic, so we avoid driving a live copy-mode yank).
        registry.push_paste_buffer(b"echoed-paste\n".to_vec()).await;
        // Ctrl+a ] pastes the newest buffer into the cat pane.
        Codec::write_frame(
            &mut cw,
            &postcard::to_allocvec(&ClientMsg::Input(bytes::Bytes::from_static(b"\x01]"))).unwrap(),
        )
        .await
        .unwrap();

        // `cat` echoes it → the pane screen shows it.
        let screen_has = |needle: &'static str| {
            let registry = Arc::clone(&registry);
            async move {
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    if let Some(s) = registry.get("main").await {
                        let m = s.window_manager.lock().await;
                        let hit = m.active_window().active_pane().map(|p| {
                            p.with_screen(|sc| {
                                let mut t = String::new();
                                for row in &sc.active.rows {
                                    for cell in &row.cells {
                                        t.push_str(cell.grapheme.as_str());
                                    }
                                }
                                t.contains(needle)
                            })
                        });
                        if hit == Some(true) {
                            return;
                        }
                    }
                    assert!(Instant::now() <= deadline, "pane never showed {needle:?}");
                    time::sleep(Duration::from_millis(20)).await;
                }
            }
        };
        screen_has("echoed-paste").await;

        // Add a second buffer; the chooser then deletes one.
        registry.push_paste_buffer(b"second\n".to_vec()).await;
        assert_eq!(registry.list_paste_buffers().await.len(), 2);
        // Ctrl+a = opens the chooser; `d` deletes the selected (newest) buffer.
        Codec::write_frame(
            &mut cw,
            &postcard::to_allocvec(&ClientMsg::Input(bytes::Bytes::from_static(b"\x01=d")))
                .unwrap(),
        )
        .await
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if registry.list_paste_buffers().await.len() == 1 {
                break;
            }
            assert!(
                Instant::now() <= deadline,
                "chooser delete did not drop the buffer count"
            );
            time::sleep(Duration::from_millis(20)).await;
        }

        server.abort();
    }

    // K2: behind a modal overlay, a paste (and raw bytes) must NOT leak to the
    // pane's child. Without an overlay the same paste reaches cat (regression
    // guard). Drives the real wire path.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn paste_swallowed_behind_overlay_but_reaches_pane_without_one() {
        let _g = isolate();
        use std::time::{Duration, Instant};

        use tokio::io::{AsyncReadExt, split};

        let registry = Arc::new(crate::SessionRegistry::new());
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        let size = PtySize {
            rows: 16,
            cols: 60,
            pixel_width: 0,
            pixel_height: 0,
        };
        let cat = || SpawnSpec {
            program: "/bin/cat".into(),
            args: vec![],
            env: vec![],
            cwd: None,
        };

        let (server_side, client_side) = duplex(64 * 1024);
        let reg = Arc::clone(&registry);
        let cfg2 = Arc::clone(&cfg);
        let server = tokio::spawn(async move { serve(server_side, 7, reg, cfg2).await });

        let (mut cr, mut cw) = split(client_side);
        let _ = client_handshake(&mut cr, &mut cw).await.unwrap();
        let attach = ClientMsg::AttachOrCreate {
            name: Some("main".into()),
            create_if_missing: true,
            cmd: Some(cat()),
            size,
        };
        Codec::write_frame(&mut cw, &postcard::to_allocvec(&attach).unwrap())
            .await
            .unwrap();
        tokio::spawn(async move {
            let mut b = [0u8; 4096];
            while cr.read(&mut b).await.unwrap_or(0) > 0 {}
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        let session = loop {
            if let Some(s) = registry.get("main").await {
                break s;
            }
            assert!(Instant::now() < deadline, "session never created");
            time::sleep(Duration::from_millis(20)).await;
        };
        let pane = session
            .window_manager
            .lock()
            .await
            .input_target_pane()
            .expect("session has a pane")
            .clone();

        // 1. No overlay: a bracketed paste reaches cat, which echoes it.
        Codec::write_frame(
            &mut cw,
            &postcard::to_allocvec(&ClientMsg::Input(bytes::Bytes::from_static(
                b"\x1b[200~no-overlay-leak\n\x1b[201~",
            )))
            .unwrap(),
        )
        .await
        .unwrap();
        wait_screen_contains(&pane, "no-overlay-leak").await;

        // 2. Open the help overlay (Ctrl+a ?), then send a paste. Behind the
        //    modal overlay it must be DISCARDED, so cat never sees it.
        Codec::write_frame(
            &mut cw,
            &postcard::to_allocvec(&ClientMsg::Input(bytes::Bytes::from_static(b"\x01?"))).unwrap(),
        )
        .await
        .unwrap();
        // Wait for the overlay to actually be open before sending the paste.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let open = { session.window_manager.lock().await.overlay().is_some() };
            if open {
                break;
            }
            assert!(Instant::now() < deadline, "help overlay never opened");
            time::sleep(Duration::from_millis(20)).await;
        }
        Codec::write_frame(
            &mut cw,
            &postcard::to_allocvec(&ClientMsg::Input(bytes::Bytes::from_static(
                b"\x1b[200~SWALLOWED-PASTE\n\x1b[201~",
            )))
            .unwrap(),
        )
        .await
        .unwrap();

        // Give the daemon ample time to (incorrectly) forward it, then assert
        // the marker never reached cat's echo.
        time::sleep(Duration::from_millis(300)).await;
        let text = pane.with_screen(plexy_glass_mux::screen_text);
        assert!(
            !text.contains("SWALLOWED-PASTE"),
            "paste leaked to the pane behind an overlay; screen:\n{text}"
        );

        server.abort();
    }

    // The feature's primary entry point: a copy-mode yank pushes a paste buffer.
    // Drives the real key path (Ctrl+a [ to enter copy mode, `v` to start a
    // selection, `y` to yank) so the yank→push wiring is protected.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn copy_mode_yank_pushes_a_paste_buffer() {
        let _g = isolate();
        use std::time::{Duration, Instant};

        use tokio::io::{AsyncReadExt, split};

        let registry = Arc::new(crate::SessionRegistry::new());
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        let size = PtySize {
            rows: 16,
            cols: 60,
            pixel_width: 0,
            pixel_height: 0,
        };
        let cat = || SpawnSpec {
            program: "/bin/cat".into(),
            args: vec![],
            env: vec![],
            cwd: None,
        };

        let (server_side, client_side) = duplex(64 * 1024);
        let reg = Arc::clone(&registry);
        let cfg2 = Arc::clone(&cfg);
        let server = tokio::spawn(async move { serve(server_side, 7, reg, cfg2).await });

        let (mut cr, mut cw) = split(client_side);
        let _ = client_handshake(&mut cr, &mut cw).await.unwrap();
        let attach = ClientMsg::AttachOrCreate {
            name: Some("main".into()),
            create_if_missing: true,
            cmd: Some(cat()),
            size,
        };
        Codec::write_frame(&mut cw, &postcard::to_allocvec(&attach).unwrap())
            .await
            .unwrap();
        tokio::spawn(async move {
            let mut b = [0u8; 4096];
            while cr.read(&mut b).await.unwrap_or(0) > 0 {}
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        while registry.get("main").await.is_none() {
            assert!(Instant::now() <= deadline, "session never created");
            time::sleep(Duration::from_millis(20)).await;
        }

        // Ctrl+a [ enters copy mode; `v` starts a selection; `y` yanks → push.
        Codec::write_frame(
            &mut cw,
            &postcard::to_allocvec(&ClientMsg::Input(bytes::Bytes::from_static(b"\x01[vy")))
                .unwrap(),
        )
        .await
        .unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if !registry.list_paste_buffers().await.is_empty() {
                break;
            }
            assert!(
                Instant::now() <= deadline,
                "copy-mode yank did not push a paste buffer"
            );
            time::sleep(Duration::from_millis(20)).await;
        }

        server.abort();
    }

    // End-to-end through the production render coordinator (the sole caller of
    // update_monitor_flags): a BEL in a BACKGROUND window flags that window
    // (monitor-bell on by default), and switching to it clears the flag.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn background_bell_flags_window_via_coordinator() {
        let _g = isolate();
        use std::time::{Duration, Instant};

        use tokio::io::{AsyncReadExt, split};

        let registry = Arc::new(crate::SessionRegistry::new());
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        let size = PtySize {
            rows: 16,
            cols: 60,
            pixel_width: 0,
            pixel_height: 0,
        };
        let cat = || SpawnSpec {
            program: "/bin/cat".into(),
            args: vec![],
            env: vec![],
            cwd: None,
        };

        let (server_side, client_side) = duplex(64 * 1024);
        let reg = Arc::clone(&registry);
        let cfg2 = Arc::clone(&cfg);
        let server = tokio::spawn(async move { serve(server_side, 7, reg, cfg2).await });

        let (mut cr, mut cw) = split(client_side);
        let _ = client_handshake(&mut cr, &mut cw).await.unwrap();
        Codec::write_frame(
            &mut cw,
            &postcard::to_allocvec(&ClientMsg::AttachOrCreate {
                name: Some("bellmon".into()),
                create_if_missing: true,
                cmd: Some(cat()),
                size,
            })
            .unwrap(),
        )
        .await
        .unwrap();
        tokio::spawn(async move {
            let mut b = [0u8; 4096];
            while cr.read(&mut b).await.unwrap_or(0) > 0 {}
        });

        // Create window 1 with the test's `cat` spec, then make window 0
        // active again (window 1 background). Do NOT create it via Ctrl+a c
        // (`Command::NewWindow`): that deliberately spawns `$SHELL`, and an
        // interactive login shell only emits a BEL if its line editor happens
        // to beep on ^G, after fork/exec plus sourcing the user's rc files,
        // which under full-suite load occasionally exceeded the 5s deadline
        // (the old flake) and breaks outright under a missing/misconfigured
        // $SHELL. cat echoes the typed BEL byte verbatim within milliseconds.
        // The bell below still flows through the full production pipeline:
        // pane reader → bell atomic → notify → render coordinator (the sole
        // caller of update_monitor_flags).
        let deadline = Instant::now() + Duration::from_secs(5);
        while registry.get("bellmon").await.is_none() {
            assert!(Instant::now() <= deadline, "session never created");
            time::sleep(Duration::from_millis(20)).await;
        }
        {
            let s = registry.get("bellmon").await.unwrap();
            let mut m = s.window_manager.lock().await;
            m.new_window_with_spec(cat(), "bg".into()).unwrap();
            m.set_active_window(0);
            assert_eq!(m.windows().len(), 2);
            assert_eq!(m.active_idx(), 0);
        }

        // Emit a real BEL in the BACKGROUND window's pane (cat outputs the \x07
        // once the newline flushes its line); the reader wakes the coordinator,
        // which is the sole caller of update_monitor_flags.
        {
            let s = registry.get("bellmon").await.unwrap();
            let m = s.window_manager.lock().await;
            let pid = m.windows()[1].layout().panes()[0];
            m.windows()[1]
                .pane(pid)
                .unwrap()
                .send_input(bytes::Bytes::from_static(b"\x07\n"))
                .await
                .unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let flagged = {
                let s = registry.get("bellmon").await.unwrap();
                let m = s.window_manager.lock().await;
                m.windows()[1].bell_flag()
            };
            if flagged {
                break;
            }
            assert!(
                Instant::now() <= deadline,
                "the coordinator never flagged the background window's bell"
            );
            time::sleep(Duration::from_millis(20)).await;
        }

        // Switching to window 1 (Ctrl+a n) clears its flag.
        Codec::write_frame(
            &mut cw,
            &postcard::to_allocvec(&ClientMsg::Input(bytes::Bytes::from_static(b"\x01n"))).unwrap(),
        )
        .await
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let cleared = {
                let s = registry.get("bellmon").await.unwrap();
                let m = s.window_manager.lock().await;
                m.active_idx() == 1 && !m.windows()[1].bell_flag()
            };
            if cleared {
                break;
            }
            assert!(
                Instant::now() <= deadline,
                "bell flag did not clear after switching to the window"
            );
            time::sleep(Duration::from_millis(20)).await;
        }

        server.abort();
    }

    // A ClientMsg::FocusIn queues `\e[I` to the active pane ONLY if that pane
    // enabled focus reporting (?1004h). A pane that never enabled it gets
    // nothing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn focus_in_reaches_only_a_subscribing_pane() {
        let _g = isolate();
        use std::time::{Duration, Instant};

        use tokio::io::{AsyncReadExt, split};

        let registry = Arc::new(crate::SessionRegistry::new());
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        let size = PtySize {
            rows: 8,
            cols: 24,
            pixel_width: 0,
            pixel_height: 0,
        };
        let cat = || SpawnSpec {
            program: "/bin/cat".into(),
            args: vec![],
            env: vec![],
            cwd: None,
        };

        let (server_side, client_side) = duplex(64 * 1024);
        let reg = Arc::clone(&registry);
        let cfg2 = Arc::clone(&cfg);
        let server = tokio::spawn(async move { serve(server_side, 7, reg, cfg2).await });

        let (mut cr, mut cw) = split(client_side);
        let _ = client_handshake(&mut cr, &mut cw).await.unwrap();
        Codec::write_frame(
            &mut cw,
            &postcard::to_allocvec(&ClientMsg::AttachOrCreate {
                name: Some("focusrt".into()),
                create_if_missing: true,
                cmd: Some(cat()),
                size,
            })
            .unwrap(),
        )
        .await
        .unwrap();
        tokio::spawn(async move {
            let mut b = [0u8; 4096];
            while cr.read(&mut b).await.unwrap_or(0) > 0 {}
        });

        // Wait for the pane to exist, then turn ON `?1004` in its emulator
        // directly (simulating the child having enabled focus reporting).
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(s) = registry.get("focusrt").await {
                let m = s.window_manager.lock().await;
                if let Some(p) = m.active_window().active_pane() {
                    p.with_screen_mut(|sc| {
                        sc.modes.insert(plexy_glass_emulator::Modes::FOCUS_EVENTS);
                    });
                    break;
                }
            }
            assert!(Instant::now() < deadline, "pane never appeared");
            time::sleep(Duration::from_millis(20)).await;
        }

        // The daemon queues `\e[I` to the child PTY. We observe it via the
        // cooked PTY's input echo: the line discipline renders the ESC control
        // byte in caret notation, so `\x1b[I` comes back on the pane's output as
        // the four printable bytes `^[[I` (b"^[[I"). Subscribe BEFORE sending so
        // we don't miss the echo.
        const ECHOED_FOCUS_IN: &[u8] = b"^[[I"; // canonical-mode echo of \x1b[I
        let got_focus_in = {
            let s = registry.get("focusrt").await.unwrap();
            let mut rx = {
                let m = s.window_manager.lock().await;
                m.active_window().active_pane().unwrap().subscribe_output()
            };
            // Send FocusIn now that we're subscribed so the echo is deterministic.
            Codec::write_frame(
                &mut cw,
                &postcard::to_allocvec(&ClientMsg::FocusIn).unwrap(),
            )
            .await
            .unwrap();
            // Accumulate across chunks: the 4-byte echo can split across PTY
            // reads, so scan a growing buffer (a per-chunk scan misses an echo
            // that straddles two PTY reads).
            let mut acc: Vec<u8> = Vec::new();
            let mut seen = false;
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                if let Ok(Ok(b)) = time::timeout(Duration::from_millis(200), rx.recv()).await {
                    acc.extend_from_slice(&b);
                    if acc
                        .windows(ECHOED_FOCUS_IN.len())
                        .any(|w| w == ECHOED_FOCUS_IN)
                    {
                        seen = true;
                        break;
                    }
                }
            }
            seen
        };
        assert!(
            got_focus_in,
            "subscribing pane never received \\e[I (as ^[[I)"
        );

        server.abort();
    }

    // A pane WITHOUT ?1004 receives no focus-in bytes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn focus_in_is_dropped_for_non_subscriber() {
        let _g = isolate();
        use std::time::{Duration, Instant};

        use tokio::io::{AsyncReadExt, split};

        let registry = Arc::new(crate::SessionRegistry::new());
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        let size = PtySize {
            rows: 8,
            cols: 24,
            pixel_width: 0,
            pixel_height: 0,
        };
        let cat = || SpawnSpec {
            program: "/bin/cat".into(),
            args: vec![],
            env: vec![],
            cwd: None,
        };

        let (server_side, client_side) = duplex(64 * 1024);
        let reg = Arc::clone(&registry);
        let cfg2 = Arc::clone(&cfg);
        let server = tokio::spawn(async move { serve(server_side, 7, reg, cfg2).await });

        let (mut cr, mut cw) = split(client_side);
        let _ = client_handshake(&mut cr, &mut cw).await.unwrap();
        Codec::write_frame(
            &mut cw,
            &postcard::to_allocvec(&ClientMsg::AttachOrCreate {
                name: Some("nofocus".into()),
                create_if_missing: true,
                cmd: Some(cat()),
                size,
            })
            .unwrap(),
        )
        .await
        .unwrap();
        tokio::spawn(async move {
            let mut b = [0u8; 4096];
            while cr.read(&mut b).await.unwrap_or(0) > 0 {}
        });

        // Wait for the pane (focus reporting left OFF).
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut rx = loop {
            if let Some(s) = registry.get("nofocus").await {
                let m = s.window_manager.lock().await;
                if let Some(p) = m.active_window().active_pane() {
                    break p.subscribe_output();
                }
            }
            assert!(Instant::now() < deadline, "pane never appeared");
            time::sleep(Duration::from_millis(20)).await;
        };

        Codec::write_frame(
            &mut cw,
            &postcard::to_allocvec(&ClientMsg::FocusIn).unwrap(),
        )
        .await
        .unwrap();

        // No focus-in should ever reach a non-subscriber; give it a generous
        // window. The echo form is `^[[I` (see the sibling test for why).
        const ECHOED_FOCUS_IN: &[u8] = b"^[[I"; // canonical-mode echo of \x1b[I
        let leaked = {
            let mut acc: Vec<u8> = Vec::new();
            let mut leaked = false;
            let deadline = Instant::now() + Duration::from_millis(600);
            while Instant::now() < deadline {
                if let Ok(Ok(b)) = time::timeout(Duration::from_millis(150), rx.recv()).await {
                    acc.extend_from_slice(&b);
                    if acc
                        .windows(ECHOED_FOCUS_IN.len())
                        .any(|w| w == ECHOED_FOCUS_IN)
                    {
                        leaked = true;
                        break;
                    }
                }
            }
            leaked
        };
        assert!(!leaked, "non-subscriber must not receive \\e[I");

        server.abort();
    }

    // A ClientMsg::ColorScheme(Dark) forwards `\e[?997;1n` to a pane ONLY if
    // that pane subscribed via ?2031 (COLOR_SCHEME_UPDATES). Mirrors the
    // focus-in test: the cooked PTY echoes the control bytes in caret notation,
    // so `\x1b[?997;1n` comes back as `^[[?997;1n`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn color_scheme_reaches_only_a_subscribing_pane() {
        let _g = isolate();
        use std::time::{Duration, Instant};

        use tokio::io::{AsyncReadExt, split};

        let registry = Arc::new(crate::SessionRegistry::new());
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        let size = PtySize {
            rows: 8,
            cols: 24,
            pixel_width: 0,
            pixel_height: 0,
        };
        let cat = || SpawnSpec {
            program: "/bin/cat".into(),
            args: vec![],
            env: vec![],
            cwd: None,
        };

        let (server_side, client_side) = duplex(64 * 1024);
        let reg = Arc::clone(&registry);
        let cfg2 = Arc::clone(&cfg);
        let server = tokio::spawn(async move { serve(server_side, 7, reg, cfg2).await });

        let (mut cr, mut cw) = split(client_side);
        let _ = client_handshake(&mut cr, &mut cw).await.unwrap();
        Codec::write_frame(
            &mut cw,
            &postcard::to_allocvec(&ClientMsg::AttachOrCreate {
                name: Some("themert".into()),
                create_if_missing: true,
                cmd: Some(cat()),
                size,
            })
            .unwrap(),
        )
        .await
        .unwrap();
        tokio::spawn(async move {
            let mut b = [0u8; 4096];
            while cr.read(&mut b).await.unwrap_or(0) > 0 {}
        });

        // Wait for the pane, then turn ON `?2031` (`COLOR_SCHEME_UPDATES`) directly.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(s) = registry.get("themert").await {
                let m = s.window_manager.lock().await;
                if let Some(p) = m.active_window().active_pane() {
                    p.with_screen_mut(|sc| {
                        sc.modes
                            .insert(plexy_glass_emulator::Modes::COLOR_SCHEME_UPDATES);
                    });
                    break;
                }
            }
            assert!(Instant::now() < deadline, "pane never appeared");
            time::sleep(Duration::from_millis(20)).await;
        }

        // Subscribe BEFORE sending so we don't miss the echo. The cooked PTY
        // echoes `\x1b[?997;1n` as the printable `^[[?997;1n`.
        const ECHOED_DARK: &[u8] = b"^[[?997;1n";
        let got = {
            let s = registry.get("themert").await.unwrap();
            let mut rx = {
                let m = s.window_manager.lock().await;
                m.active_window().active_pane().unwrap().subscribe_output()
            };
            Codec::write_frame(
                &mut cw,
                &postcard::to_allocvec(&ClientMsg::ColorScheme(
                    plexy_glass_protocol::ColorScheme::Dark,
                ))
                .unwrap(),
            )
            .await
            .unwrap();
            let mut acc: Vec<u8> = Vec::new();
            let mut seen = false;
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                if let Ok(Ok(b)) = time::timeout(Duration::from_millis(200), rx.recv()).await {
                    acc.extend_from_slice(&b);
                    if acc.windows(ECHOED_DARK.len()).any(|w| w == ECHOED_DARK) {
                        seen = true;
                        break;
                    }
                }
            }
            seen
        };
        assert!(
            got,
            "subscribing pane never received \\e[?997;1n (as ^[[?997;1n)"
        );

        server.abort();
    }

    // A pane WITHOUT ?2031 receives no color-scheme report.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn color_scheme_is_dropped_for_non_subscriber() {
        let _g = isolate();
        use std::time::{Duration, Instant};

        use tokio::io::{AsyncReadExt, split};

        let registry = Arc::new(crate::SessionRegistry::new());
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        let size = PtySize {
            rows: 8,
            cols: 24,
            pixel_width: 0,
            pixel_height: 0,
        };
        let cat = || SpawnSpec {
            program: "/bin/cat".into(),
            args: vec![],
            env: vec![],
            cwd: None,
        };

        let (server_side, client_side) = duplex(64 * 1024);
        let reg = Arc::clone(&registry);
        let cfg2 = Arc::clone(&cfg);
        let server = tokio::spawn(async move { serve(server_side, 7, reg, cfg2).await });

        let (mut cr, mut cw) = split(client_side);
        let _ = client_handshake(&mut cr, &mut cw).await.unwrap();
        Codec::write_frame(
            &mut cw,
            &postcard::to_allocvec(&ClientMsg::AttachOrCreate {
                name: Some("notheme".into()),
                create_if_missing: true,
                cmd: Some(cat()),
                size,
            })
            .unwrap(),
        )
        .await
        .unwrap();
        tokio::spawn(async move {
            let mut b = [0u8; 4096];
            while cr.read(&mut b).await.unwrap_or(0) > 0 {}
        });

        // Wait for the pane (`?2031` left OFF).
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut rx = loop {
            if let Some(s) = registry.get("notheme").await {
                let m = s.window_manager.lock().await;
                if let Some(p) = m.active_window().active_pane() {
                    break p.subscribe_output();
                }
            }
            assert!(Instant::now() < deadline, "pane never appeared");
            time::sleep(Duration::from_millis(20)).await;
        };

        Codec::write_frame(
            &mut cw,
            &postcard::to_allocvec(&ClientMsg::ColorScheme(
                plexy_glass_protocol::ColorScheme::Dark,
            ))
            .unwrap(),
        )
        .await
        .unwrap();

        const ECHOED_DARK: &[u8] = b"^[[?997;1n";
        let leaked = {
            let mut acc: Vec<u8> = Vec::new();
            let mut leaked = false;
            let deadline = Instant::now() + Duration::from_millis(600);
            while Instant::now() < deadline {
                if let Ok(Ok(b)) = time::timeout(Duration::from_millis(150), rx.recv()).await {
                    acc.extend_from_slice(&b);
                    if acc.windows(ECHOED_DARK.len()).any(|w| w == ECHOED_DARK) {
                        leaked = true;
                        break;
                    }
                }
            }
            leaked
        };
        assert!(!leaked, "non-subscriber must not receive \\e[?997;1n");

        server.abort();
    }

    // A pane SWITCH between two ?1004 subscribers synthesizes a focus
    // transition: the pane we move TO receives `\e[I`. This exercises the
    // batch-level snapshot in the input loop via `select_last_pane`
    // (`Ctrl+a ;`), which routes through `handle_command` and never touched
    // the old per-call-site machinery, the exact gap the refactor closes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pane_switch_synthesizes_focus_in_to_destination() {
        let _g = isolate();
        use std::time::{Duration, Instant};

        use tokio::io::{AsyncReadExt, split};

        let registry = Arc::new(crate::SessionRegistry::new());
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        let size = PtySize {
            rows: 8,
            cols: 24,
            pixel_width: 0,
            pixel_height: 0,
        };
        let cat = || SpawnSpec {
            program: "/bin/cat".into(),
            args: vec![],
            env: vec![],
            cwd: None,
        };

        let (server_side, client_side) = duplex(64 * 1024);
        let reg = Arc::clone(&registry);
        let cfg2 = Arc::clone(&cfg);
        let server = tokio::spawn(async move { serve(server_side, 7, reg, cfg2).await });

        let (mut cr, mut cw) = split(client_side);
        let _ = client_handshake(&mut cr, &mut cw).await.unwrap();
        Codec::write_frame(
            &mut cw,
            &postcard::to_allocvec(&ClientMsg::AttachOrCreate {
                name: Some("focusswap".into()),
                create_if_missing: true,
                cmd: Some(cat()),
                size,
            })
            .unwrap(),
        )
        .await
        .unwrap();
        tokio::spawn(async move {
            let mut b = [0u8; 4096];
            while cr.read(&mut b).await.unwrap_or(0) > 0 {}
        });

        // Wait for the first pane to exist.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(s) = registry.get("focusswap").await {
                let m = s.window_manager.lock().await;
                if m.active_window().active_pane().is_some() {
                    break;
                }
            }
            assert!(Instant::now() < deadline, "first pane never appeared");
            time::sleep(Duration::from_millis(20)).await;
        }

        // Ctrl+a v → split into two panes (the new pane becomes active).
        Codec::write_frame(
            &mut cw,
            &postcard::to_allocvec(&ClientMsg::Input(bytes::Bytes::from_static(b"\x01v"))).unwrap(),
        )
        .await
        .unwrap();

        // Wait for the second pane, then enable `?1004` (`FOCUS_EVENTS`) on BOTH.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let s = registry.get("focusswap").await.unwrap();
            let st = s.tree_snapshot().await;
            if st.total_panes == 2 {
                let m = s.window_manager.lock().await;
                for (_id, p) in m.active_window().panes() {
                    p.with_screen_mut(|sc| {
                        sc.modes.insert(plexy_glass_emulator::Modes::FOCUS_EVENTS);
                    });
                }
                break;
            }
            assert!(Instant::now() < deadline, "split never produced 2 panes");
            time::sleep(Duration::from_millis(20)).await;
        }

        // Subscribe to PaneId(0) (the pane we will switch BACK to) BEFORE the
        // switch so the echo isn't missed. The new (active) pane is PaneId(1).
        const ECHOED_FOCUS_IN: &[u8] = b"^[[I"; // canonical-mode echo of \x1b[I
        let got_focus_in = {
            let s = registry.get("focusswap").await.unwrap();
            let mut rx = {
                let m = s.window_manager.lock().await;
                m.active_window()
                    .pane(plexy_glass_mux::PaneId(0))
                    .unwrap()
                    .subscribe_output()
            };
            // Ctrl+a ; → select_last_pane, switching the active pane from
            // PaneId(1) back to PaneId(0); the batch snapshot then synthesizes
            // focus-out(1)/focus-in(0).
            Codec::write_frame(
                &mut cw,
                &postcard::to_allocvec(&ClientMsg::Input(bytes::Bytes::from_static(b"\x01;")))
                    .unwrap(),
            )
            .await
            .unwrap();
            // Accumulate across chunks: the 4-byte echo can straddle two PTY
            // reads, so scan a growing buffer.
            let mut acc: Vec<u8> = Vec::new();
            let mut seen = false;
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                if let Ok(Ok(b)) = time::timeout(Duration::from_millis(200), rx.recv()).await {
                    acc.extend_from_slice(&b);
                    if acc
                        .windows(ECHOED_FOCUS_IN.len())
                        .any(|w| w == ECHOED_FOCUS_IN)
                    {
                        seen = true;
                        break;
                    }
                }
            }
            seen
        };
        assert!(
            got_focus_in,
            "switched-to pane never received \\e[I (as ^[[I)"
        );

        server.abort();
    }

    // ── one-shot scripting verbs (RunCommand / SendInput / CapturePane) ─────

    fn script_cat() -> SpawnSpec {
        SpawnSpec {
            program: "/bin/cat".into(),
            args: vec![],
            env: vec![],
            cwd: None,
        }
    }

    fn script_size() -> PtySize {
        PtySize {
            rows: 8,
            cols: 40,
            pixel_width: 0,
            pixel_height: 0,
        }
    }

    /// Drive one one-shot scripting message through `serve` over a
    /// duplex (handshake → one frame → one reply), like a CLI invocation does.
    async fn one_shot(registry: &Arc<crate::SessionRegistry>, msg: &ClientMsg) -> ServerMsg {
        let (server_side, client_side) = duplex(64 * 1024);
        let reg = Arc::clone(registry);
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        let server = tokio::spawn(async move { serve(server_side, 7, reg, cfg).await });
        let (mut cr, mut cw) = io::split(client_side);
        let _ = client_handshake(&mut cr, &mut cw).await.unwrap();
        Codec::write_frame(&mut cw, &postcard::to_allocvec(msg).unwrap())
            .await
            .unwrap();
        let frame = time::timeout(Duration::from_secs(5), Codec::read_frame(&mut cr))
            .await
            .expect("one-shot reply timed out")
            .unwrap()
            .expect("server closed without replying");
        let reply: ServerMsg = postcard::from_bytes(&frame).unwrap();
        server.await.unwrap().unwrap();
        reply
    }

    fn expect_command_result(reply: ServerMsg) -> (bool, Option<String>) {
        match reply {
            ServerMsg::CommandResult { ok, message } => (ok, message),
            other => panic!("expected CommandResult, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_command_splits_via_wire() {
        let _g = isolate();
        let registry = Arc::new(crate::SessionRegistry::new());
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        let session = registry
            .attach_or_create("s1".into(), script_cat(), script_size(), cfg)
            .await
            .unwrap();
        // Pin the split's spawn program so it never depends on `$SHELL`.
        session
            .window_manager
            .lock()
            .await
            .set_default_program("/bin/cat");

        let reply = one_shot(
            &registry,
            &ClientMsg::RunCommand {
                session: Some("s1".into()),
                line: "split v".into(),
            },
        )
        .await;
        let (ok, message) = expect_command_result(reply);
        assert!(ok, "split v over the wire failed: {message:?}");
        assert_eq!(
            session
                .window_manager
                .lock()
                .await
                .active_window()
                .layout()
                .panes()
                .len(),
            2
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_command_parse_error_is_not_ok() {
        let _g = isolate();
        let registry = Arc::new(crate::SessionRegistry::new());
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        registry
            .attach_or_create("s1".into(), script_cat(), script_size(), cfg)
            .await
            .unwrap();

        let reply = one_shot(
            &registry,
            &ClientMsg::RunCommand {
                session: Some("s1".into()),
                line: "bogusverb".into(),
            },
        )
        .await;
        let (ok, message) = expect_command_result(reply);
        assert!(!ok);
        let msg = message.expect("parse error must carry a message");
        assert!(
            msg.contains("unknown command"),
            "unexpected parse error text: {msg}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_command_interactive_only_refused() {
        let _g = isolate();
        let registry = Arc::new(crate::SessionRegistry::new());
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        registry
            .attach_or_create("s1".into(), script_cat(), script_size(), cfg)
            .await
            .unwrap();

        for line in ["detach", "switch x", "help", "sessions", "tree", "buffers"] {
            let reply = one_shot(
                &registry,
                &ClientMsg::RunCommand {
                    session: Some("s1".into()),
                    line: line.into(),
                },
            )
            .await;
            let (ok, message) = expect_command_result(reply);
            assert!(!ok, "interactive-only `{line}` was accepted headless");
            let msg = message.unwrap_or_default();
            assert!(
                msg.contains("requires an attached client"),
                "`{line}` refusal text wrong: {msg}"
            );
        }
    }

    /// Lockstep guard for the headless verb policy.
    /// `Session::handle_prompt_command` carries a defensive `Ok(None)` arm for
    /// the connection-level verbs, currently thirteen: Detach, Reload, Switch,
    /// ChooseSession, ChooseTree, History, Hints, PasteBuffer, ChooseBuffer,
    /// CopyOutput, SetBuffer, SaveBuffer, LoadBuffer. If a future verb is added
    /// to that arm (and `ConnVerb::from_prompt`) but NOT to `run_prompt_line`'s
    /// intercept, `cmd "<verb>"` would fall through to the defensive arm and
    /// silently exit 0 doing nothing. This test hardcodes the current twelve
    /// (plus `help`, refused for its modal overlay) and asserts each is
    /// either refused with a message or specially handled with a real effect,
    /// never a silent no-op. If you add a connection-level verb, extend
    /// BOTH `run_prompt_line` and this test.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_prompt_line_never_silently_noops_connection_verbs() {
        let _g = isolate();
        let registry = Arc::new(crate::SessionRegistry::new());
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        let session = registry
            .attach_or_create("guard".into(), script_cat(), script_size(), cfg)
            .await
            .unwrap();

        // Refused: these act on the calling client (detach/switch), open modal
        // overlays (help/sessions/tree/buffers), or are interactive per-pane
        // modal navigation (block-mode).
        for line in [
            "detach",
            "switch x",
            "sessions",
            "tree",
            "history",
            "hints",
            "palette",
            "buffers",
            "help",
            "block-mode",
        ] {
            let (ok, message) = run_prompt_line(&session, &registry, line).await;
            assert!(
                !ok,
                "`{line}` must be refused headless, not silently succeed"
            );
            let msg = message.unwrap_or_default();
            assert!(
                msg.contains("requires an attached client"),
                "`{line}` refusal text wrong: {msg}"
            );
        }

        // Specially handled with a real effect (not the defensive no-op):
        // `reload` re-reads config through the registry (a missing-or-valid
        // config file is Ok, the same dependency as
        // `registry::tests::reload_config_swaps_session_config`), and `paste`
        // pastes the top buffer (or sets a "no paste buffer" status).
        for line in ["reload", "paste"] {
            let (ok, message) = run_prompt_line(&session, &registry, line).await;
            assert!(ok, "headless `{line}` failed: {message:?}");
        }

        // `copy-output` is specially handled too: with no OSC 133 blocks in
        // the cat pane it must FAIL with the no-blocks message, a real
        // effect, never the silent defensive no-op.
        let (ok, message) = run_prompt_line(&session, &registry, "copy-output").await;
        assert!(!ok, "copy-output with no blocks must not claim success");
        let msg = message.unwrap_or_default();
        assert!(
            msg.contains("no command blocks"),
            "wrong no-blocks text: {msg}"
        );

        // The buffer-file verbs are specially handled with real effects:
        // set pushes (with a confirmation message), save writes the file,
        // load reads it back, and a by-name paste of an unknown buffer
        // FAILS with a message, never the silent defensive no-op.
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("guard.txt");
        let (ok, message) = run_prompt_line(&session, &registry, "set-buffer guard text").await;
        assert!(ok, "headless set-buffer failed: {message:?}");
        assert!(
            message.unwrap_or_default().contains("buffer set"),
            "set-buffer must confirm"
        );
        let (ok, message) = run_prompt_line(
            &session,
            &registry,
            &format!("save-buffer {}", out.display()),
        )
        .await;
        assert!(ok, "headless save-buffer failed: {message:?}");
        assert!(
            message.unwrap_or_default().contains("saved "),
            "save-buffer must confirm with 'saved …'"
        );
        let (ok, message) = run_prompt_line(
            &session,
            &registry,
            &format!("load-buffer {}", out.display()),
        )
        .await;
        assert!(ok, "headless load-buffer failed: {message:?}");
        assert!(
            message.unwrap_or_default().contains("loaded "),
            "load-buffer must confirm with 'loaded …'"
        );
        let (ok, message) = run_prompt_line(&session, &registry, "paste buffer999").await;
        assert!(!ok, "paste of an unknown buffer must not claim success");
        let msg = message.unwrap_or_default();
        assert!(
            msg.contains("no buffer named buffer999"),
            "wrong unknown-name text: {msg}"
        );
    }

    // ── paste buffers v2: set-buffer / save-buffer / load-buffer / paste-by-name ──

    /// `set-buffer` (over the wire) pushes the rest of the line VERBATIM
    /// (internal spaces preserved), and `paste bufferN` types that buffer into
    /// the input-target pane.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn set_buffer_via_wire_then_paste_by_name_types_it() {
        let _g = isolate();
        let registry = Arc::new(crate::SessionRegistry::new());
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        let session = registry
            .attach_or_create("s1".into(), script_cat(), script_size(), cfg)
            .await
            .unwrap();

        let reply = one_shot(
            &registry,
            &ClientMsg::RunCommand {
                session: Some("s1".into()),
                line: "set-buffer hello   world".into(),
            },
        )
        .await;
        let (ok, message) = expect_command_result(reply);
        assert!(ok, "set-buffer over the wire failed: {message:?}");
        assert_eq!(message.as_deref(), Some("buffer set (13 bytes)"));

        let entries = registry.list_paste_buffers().await;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "buffer0");
        assert!(
            entries[0].preview.contains("hello   world"),
            "internal spaces must survive: {:?}",
            entries[0].preview
        );

        let reply = one_shot(
            &registry,
            &ClientMsg::RunCommand {
                session: Some("s1".into()),
                line: "paste buffer0".into(),
            },
        )
        .await;
        let (ok, message) = expect_command_result(reply);
        assert!(ok, "paste buffer0 over the wire failed: {message:?}");
        // `cat` echoes the pasted bytes, but the emulator buffers the trailing
        // grapheme until the next byte, so probe for all but the last char.
        let pane = session
            .window_manager
            .lock()
            .await
            .input_target_pane()
            .expect("session has a pane")
            .clone();
        wait_screen_contains(&pane, "hello   worl").await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn paste_unknown_name_is_an_error() {
        let _g = isolate();
        let registry = Arc::new(crate::SessionRegistry::new());
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        let session = registry
            .attach_or_create("s1".into(), script_cat(), script_size(), cfg)
            .await
            .unwrap();
        registry.push_paste_buffer(b"x".to_vec()).await;

        let (ok, message) = run_prompt_line(&session, &registry, "paste buffer42").await;
        assert!(!ok);
        assert_eq!(message.as_deref(), Some("paste: no buffer named buffer42"));
    }

    /// `save-buffer <path>` writes the NEWEST buffer; `save-buffer bufferN
    /// <path>` writes that buffer. Bytes verbatim in both (incl. a non-UTF8
    /// byte), and the status message carries the buffer name + resolved path.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn save_buffer_newest_and_named_write_bytes_verbatim() {
        let _g = isolate();
        let registry = Arc::new(crate::SessionRegistry::new());
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        let session = registry
            .attach_or_create("s1".into(), script_cat(), script_size(), cfg)
            .await
            .unwrap();
        let dir = tempfile::tempdir().unwrap();

        registry.push_paste_buffer(b"old\xFFbytes".to_vec()).await; // buffer0
        registry.push_paste_buffer(b"new line\n".to_vec()).await; // buffer1

        // Newest (`buffer1`) by default.
        let newest = dir.path().join("newest.out");
        let line = format!("save-buffer {}", newest.display());
        let (ok, message) = run_prompt_line(&session, &registry, &line).await;
        assert!(ok, "save-buffer (newest) failed: {message:?}");
        assert_eq!(
            message.as_deref(),
            Some(format!("saved buffer1 → {} (9 bytes)", newest.display()).as_str())
        );
        assert_eq!(fs::read(&newest).unwrap(), b"new line\n");

        // Named `buffer0`, binary-safe write.
        let named = dir.path().join("named.out");
        let line = format!("save-buffer buffer0 {}", named.display());
        let (ok, message) = run_prompt_line(&session, &registry, &line).await;
        assert!(ok, "save-buffer buffer0 failed: {message:?}");
        assert_eq!(fs::read(&named).unwrap(), b"old\xFFbytes");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn save_buffer_errors_carry_the_resolved_path() {
        let _g = isolate();
        let registry = Arc::new(crate::SessionRegistry::new());
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        let session = registry
            .attach_or_create("s1".into(), script_cat(), script_size(), cfg)
            .await
            .unwrap();
        let dir = tempfile::tempdir().unwrap();

        // No buffers at all.
        let (ok, message) = run_prompt_line(&session, &registry, "save-buffer /tmp/x.out").await;
        assert!(!ok);
        assert_eq!(message.as_deref(), Some("save-buffer: no paste buffer"));

        registry.push_paste_buffer(b"x".to_vec()).await;

        // Unknown buffer name.
        let line = format!(
            "save-buffer buffer99 {}",
            dir.path().join("x.out").display()
        );
        let (ok, message) = run_prompt_line(&session, &registry, &line).await;
        assert!(!ok);
        assert_eq!(
            message.as_deref(),
            Some("save-buffer: no buffer named buffer99")
        );

        // io error: the message names the RESOLVED path and the os error.
        let missing = dir.path().join("no-such-subdir").join("x.out");
        let line = format!("save-buffer {}", missing.display());
        let (ok, message) = run_prompt_line(&session, &registry, &line).await;
        assert!(!ok);
        let msg = message.unwrap_or_default();
        assert!(
            msg.starts_with(&format!("save-buffer: {}: ", missing.display())),
            "io error must carry the resolved path: {msg}"
        );

        // Relative paths are refused (after tilde expansion).
        let (ok, message) = run_prompt_line(&session, &registry, "save-buffer rel/x.out").await;
        assert!(!ok);
        assert_eq!(
            message.as_deref(),
            Some(
                "save-buffer: relative paths are not supported — the daemon's \
                 working directory is not yours; use an absolute or ~ path"
            )
        );
    }

    /// `load-buffer` reads file bytes verbatim (incl. non-UTF8) into a new
    /// newest buffer; an empty file loads as an empty buffer (legal).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn load_buffer_reads_file_bytes_verbatim() {
        let _g = isolate();
        let registry = Arc::new(crate::SessionRegistry::new());
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        let session = registry
            .attach_or_create("s1".into(), script_cat(), script_size(), cfg)
            .await
            .unwrap();
        let dir = tempfile::tempdir().unwrap();

        let file = dir.path().join("snippet.bin");
        fs::write(&file, b"bin\xFFary\ncontent").unwrap();
        let line = format!("load-buffer {}", file.display());
        let (ok, message) = run_prompt_line(&session, &registry, &line).await;
        assert!(ok, "load-buffer failed: {message:?}");
        assert_eq!(
            message.as_deref(),
            Some(format!("loaded {} (15 bytes)", file.display()).as_str())
        );
        assert_eq!(
            registry.paste_buffer_top().await.as_deref(),
            Some(b"bin\xFFary\ncontent".as_slice())
        );

        // Empty file → empty buffer.
        let empty = dir.path().join("empty.txt");
        fs::write(&empty, b"").unwrap();
        let line = format!("load-buffer {}", empty.display());
        let (ok, message) = run_prompt_line(&session, &registry, &line).await;
        assert!(ok, "load-buffer of an empty file failed: {message:?}");
        assert_eq!(
            registry.paste_buffer_top().await.as_deref(),
            Some(b"".as_slice())
        );
    }

    /// The load gates: FIFOs, directories, oversize files, and relative
    /// paths are all refused BEFORE any read.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn load_buffer_refuses_non_regular_oversize_and_relative() {
        let _g = isolate();
        let registry = Arc::new(crate::SessionRegistry::new());
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        let session = registry
            .attach_or_create("s1".into(), script_cat(), script_size(), cfg)
            .await
            .unwrap();
        let dir = tempfile::tempdir().unwrap();

        // Directory.
        let line = format!("load-buffer {}", dir.path().display());
        let (ok, message) = run_prompt_line(&session, &registry, &line).await;
        assert!(!ok);
        assert_eq!(
            message.as_deref(),
            Some(format!("load-buffer: {}: not a regular file", dir.path().display()).as_str())
        );

        // FIFO: opening it would hang a runtime worker, so the `is_file`
        // gate refuses it from metadata alone.
        let fifo = dir.path().join("pipe");
        let status = process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("spawn mkfifo");
        assert!(status.success(), "mkfifo failed");
        let line = format!("load-buffer {}", fifo.display());
        let (ok, message) = run_prompt_line(&session, &registry, &line).await;
        assert!(!ok);
        assert_eq!(
            message.as_deref(),
            Some(format!("load-buffer: {}: not a regular file", fifo.display()).as_str())
        );

        // Oversize: a sparse file one byte past the 10 MiB cap.
        let big = dir.path().join("big.bin");
        let f = fs::File::create(&big).unwrap();
        f.set_len(10 * 1024 * 1024 + 1).unwrap();
        drop(f);
        let line = format!("load-buffer {}", big.display());
        let (ok, message) = run_prompt_line(&session, &registry, &line).await;
        assert!(!ok);
        assert_eq!(
            message.as_deref(),
            Some(
                format!(
                    "load-buffer: {} is 10485761 bytes (limit 10 MiB)",
                    big.display()
                )
                .as_str()
            )
        );

        // Relative path.
        let (ok, message) = run_prompt_line(&session, &registry, "load-buffer rel.txt").await;
        assert!(!ok);
        assert_eq!(
            message.as_deref(),
            Some(
                "load-buffer: relative paths are not supported — the daemon's \
                 working directory is not yours; use an absolute or ~ path"
            )
        );
        assert!(
            registry.list_paste_buffers().await.is_empty(),
            "no refusal may push a buffer"
        );
    }

    /// A symlink to a regular file is followed (the headline case is
    /// `load-buffer ~/snippets/deploy.sh` in a symlink-managed home).
    /// A symlink to a FIFO is still refused because `metadata()` follows the
    /// link and the target fails `is_file()`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn load_buffer_follows_symlink_to_regular_file() {
        let _g = isolate();
        let registry = Arc::new(crate::SessionRegistry::new());
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        let session = registry
            .attach_or_create("s1".into(), script_cat(), script_size(), cfg)
            .await
            .unwrap();
        let dir = tempfile::tempdir().unwrap();

        // Symlink to a regular file, so this must load.
        let target = dir.path().join("real.txt");
        fs::write(&target, b"via symlink").unwrap();
        let link = dir.path().join("link.txt");
        symlink(&target, &link).unwrap();
        let line = format!("load-buffer {}", link.display());
        let (ok, message) = run_prompt_line(&session, &registry, &line).await;
        assert!(ok, "symlink-to-regular-file must load: {message:?}");
        assert_eq!(
            registry.paste_buffer_top().await.as_deref(),
            Some(b"via symlink".as_slice())
        );

        // Symlink to a FIFO, so this must be refused.
        let fifo = dir.path().join("pipe");
        let status = process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("spawn mkfifo");
        assert!(status.success(), "mkfifo failed");
        let fifo_link = dir.path().join("pipe_link");
        symlink(&fifo, &fifo_link).unwrap();
        let line = format!("load-buffer {}", fifo_link.display());
        let (ok, message) = run_prompt_line(&session, &registry, &line).await;
        assert!(!ok, "symlink-to-FIFO must be refused");
        assert_eq!(
            message.as_deref(),
            Some(format!("load-buffer: {}: not a regular file", fifo_link.display()).as_str())
        );
    }

    /// Write ASCII `text` into grid row `row` of a screen (test fixture: the
    /// cat child never produces marked output, so block tests paint the grid
    /// and set the row marks the real OSC 133 handlers would).
    fn write_grid_row(s: &mut plexy_glass_emulator::Screen, row: usize, text: &str) {
        for (i, ch) in text.chars().enumerate() {
            s.active.rows[row].cells[i].grapheme = ch.to_string().into();
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn copy_output_pushes_buffer_and_sets_status() {
        use plexy_glass_emulator::RowMark;
        let _g = isolate();
        let registry = Arc::new(crate::SessionRegistry::new());
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        let session = registry
            .attach_or_create("blocks".into(), script_cat(), script_size(), cfg)
            .await
            .unwrap();
        {
            let m = session.window_manager.lock().await;
            let pane = m.active_window().active_pane().unwrap();
            pane.with_screen_mut(|s| {
                // Block 1: prompt row 0, output "ok" on row 1; row 2 carries
                // the D (closing block 1) plus the next prompt's A, the
                // common shell flow.
                write_grid_row(s, 0, "$ make");
                s.active.rows[0].mark.set(RowMark::PROMPT_START);
                write_grid_row(s, 1, "ok");
                s.active.rows[1].mark.set(RowMark::OUTPUT_START);
                write_grid_row(s, 2, "$ next");
                s.active.rows[2].mark.set(RowMark::PROMPT_START);
                s.active.rows[2].mark.set(RowMark::BLOCK_END);
                s.active.rows[2].mark.set_exit(Some(0));
            });
        }

        let (ok, message) = run_prompt_line(&session, &registry, "copy-output").await;
        assert!(ok, "copy-output with a completed block failed: {message:?}");
        assert_eq!(
            registry.paste_buffer_top().await.as_deref(),
            Some(b"ok".as_slice()),
            "the block's output text must be pushed as a paste buffer"
        );
        let mut m = session.window_manager.lock().await;
        assert_eq!(
            m.take_active_message(),
            Some("copied output of last command")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn copy_output_no_blocks_over_the_wire_is_not_ok() {
        let _g = isolate();
        let registry = Arc::new(crate::SessionRegistry::new());
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        registry
            .attach_or_create("s1".into(), script_cat(), script_size(), cfg)
            .await
            .unwrap();

        let reply = one_shot(
            &registry,
            &ClientMsg::RunCommand {
                session: Some("s1".into()),
                line: "copy-output".into(),
            },
        )
        .await;
        let (ok, message) = expect_command_result(reply);
        assert!(!ok, "copy-output with no blocks must exit non-zero");
        let msg = message.expect("no-blocks failure must carry a message");
        assert!(msg.contains("no command blocks"), "wrong text: {msg}");
        assert!(
            registry.paste_buffer_top().await.is_none(),
            "no buffer may be pushed on the no-blocks path"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_command_reload_is_ok_via_wire() {
        let _g = isolate();
        let registry = Arc::new(crate::SessionRegistry::new());
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        registry
            .attach_or_create("s1".into(), script_cat(), script_size(), cfg)
            .await
            .unwrap();

        // `load_or_default` treats a missing config file as Ok-with-defaults
        // (`load.rs`), so a reload succeeds without any config on disk.
        let reply = one_shot(
            &registry,
            &ClientMsg::RunCommand {
                session: Some("s1".into()),
                line: "reload".into(),
            },
        )
        .await;
        let (ok, message) = expect_command_result(reply);
        assert!(ok, "reload over the wire failed: {message:?}");
    }

    // Dispatch-error coverage. No prompt verb errs naturally in a fresh
    // session: `win 9` out-of-range is a silent no-op (`switch_to_window`
    // bounds-checks and returns), and join/swap/break with no marked pane set
    // a status message and return Ok. The deterministic dispatch Err is a
    // spawn failure: pin the default program to a nonexistent path so
    // `split v` fails inside `WindowManager::handle_command`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_command_dispatch_error_is_not_ok() {
        let _g = isolate();
        let registry = Arc::new(crate::SessionRegistry::new());
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        let session = registry
            .attach_or_create("s1".into(), script_cat(), script_size(), cfg)
            .await
            .unwrap();
        session
            .window_manager
            .lock()
            .await
            .set_default_program("/nonexistent/plexy-glass-no-such-shell");

        let reply = one_shot(
            &registry,
            &ClientMsg::RunCommand {
                session: Some("s1".into()),
                line: "split v".into(),
            },
        )
        .await;
        let (ok, message) = expect_command_result(reply);
        assert!(!ok, "split with an unspawnable program must report failure");
        let msg = message.expect("dispatch error must carry a message");
        assert!(
            msg.contains("spawn"),
            "unexpected dispatch error text: {msg}"
        );
        // The failed split must not have left a half-created pane behind.
        assert_eq!(
            session
                .window_manager
                .lock()
                .await
                .active_window()
                .layout()
                .panes()
                .len(),
            1
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_command_resolution() {
        let _g = isolate();
        let registry = Arc::new(crate::SessionRegistry::new());
        let cfg = Arc::new(plexy_glass_config::built_in_default());

        // Zero sessions + no name.
        let reply = one_shot(
            &registry,
            &ClientMsg::RunCommand {
                session: None,
                line: "split v".into(),
            },
        )
        .await;
        let (ok, message) = expect_command_result(reply);
        assert!(!ok);
        assert!(message.unwrap_or_default().contains("no sessions"));

        // Explicit miss.
        let reply = one_shot(
            &registry,
            &ClientMsg::RunCommand {
                session: Some("nope".into()),
                line: "split v".into(),
            },
        )
        .await;
        let (ok, message) = expect_command_result(reply);
        assert!(!ok);
        assert!(message.unwrap_or_default().contains("no session \"nope\""));

        // Sole session + no name → resolves to it.
        let session_a = registry
            .attach_or_create("a".into(), script_cat(), script_size(), Arc::clone(&cfg))
            .await
            .unwrap();
        session_a
            .window_manager
            .lock()
            .await
            .set_default_program("/bin/cat");
        let reply = one_shot(
            &registry,
            &ClientMsg::RunCommand {
                session: None,
                line: "split v".into(),
            },
        )
        .await;
        let (ok, message) = expect_command_result(reply);
        assert!(ok, "sole-session resolution failed: {message:?}");
        assert_eq!(
            session_a
                .window_manager
                .lock()
                .await
                .active_window()
                .layout()
                .panes()
                .len(),
            2
        );

        // Two sessions + no name → ambiguous, both names listed.
        registry
            .attach_or_create("b".into(), script_cat(), script_size(), Arc::clone(&cfg))
            .await
            .unwrap();
        let reply = one_shot(
            &registry,
            &ClientMsg::RunCommand {
                session: None,
                line: "split v".into(),
            },
        )
        .await;
        let (ok, message) = expect_command_result(reply);
        assert!(!ok);
        let msg = message.unwrap_or_default();
        assert!(
            msg.contains('a') && msg.contains('b'),
            "ambiguity must list names: {msg}"
        );
        assert!(
            msg.contains("multiple sessions"),
            "unexpected ambiguity text: {msg}"
        );
    }

    /// Poll `pane`'s screen until `screen_text` contains `marker`.
    async fn wait_screen_contains(pane: &Pane, marker: &str) {
        use std::time::{Duration, Instant};
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let text = pane.with_screen(plexy_glass_mux::screen_text);
            if text.contains(marker) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "screen never showed {marker:?}; got:\n{text}"
            );
            time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn send_input_reaches_pane() {
        let _g = isolate();
        let registry = Arc::new(crate::SessionRegistry::new());
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        let session = registry
            .attach_or_create("si".into(), script_cat(), script_size(), cfg)
            .await
            .unwrap();

        let reply = one_shot(
            &registry,
            &ClientMsg::SendInput {
                session: Some("si".into()),
                bytes: bytes::Bytes::from_static(b"wire_marker\n"),
            },
        )
        .await;
        let (ok, message) = expect_command_result(reply);
        assert!(ok, "SendInput failed: {message:?}");

        // `cat` echoes the line back; it must land on the focused pane's screen.
        let pane = session
            .window_manager
            .lock()
            .await
            .input_target_pane()
            .expect("session has a pane")
            .clone();
        wait_screen_contains(&pane, "wire_marker").await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn capture_pane_returns_screen_text() {
        let _g = isolate();
        use std::time::{Duration, Instant};
        let registry = Arc::new(crate::SessionRegistry::new());
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        registry
            .attach_or_create("cap".into(), script_cat(), script_size(), cfg)
            .await
            .unwrap();

        // Content arrives via the real path: send to cat over the wire, then
        // poll capture (point-in-time) until the echo shows up.
        let reply = one_shot(
            &registry,
            &ClientMsg::SendInput {
                session: Some("cap".into()),
                bytes: bytes::Bytes::from_static(b"capture_marker\n"),
            },
        )
        .await;
        assert!(expect_command_result(reply).0);

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let reply = one_shot(
                &registry,
                &ClientMsg::CapturePane {
                    session: Some("cap".into()),
                },
            )
            .await;
            let text = match reply {
                ServerMsg::PaneCapture { text } => text,
                other => panic!("expected PaneCapture, got {other:?}"),
            };
            if text.contains("capture_marker") {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "capture never showed the marker; got:\n{text}"
            );
            time::sleep(Duration::from_millis(20)).await;
        }
    }

    // With a popup open, send/capture address the POPUP pane (the input
    // target), not the layout pane underneath, so the write and the read
    // stay symmetric.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn send_and_capture_are_popup_aware() {
        let _g = isolate();
        use std::time::{Duration, Instant};
        let registry = Arc::new(crate::SessionRegistry::new());
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        let session = registry
            .attach_or_create("pop".into(), script_cat(), script_size(), cfg)
            .await
            .unwrap();
        // Pin the popup's `$SHELL -c …` to `/bin/sh` so it never depends on the user's shell.
        session
            .window_manager
            .lock()
            .await
            .set_default_program("/bin/sh");
        session
            .handle_command(plexy_glass_mux::Command::OpenPopup {
                command: Some("cat".into()),
            })
            .await
            .unwrap();
        let popup_pane = {
            let m = session.window_manager.lock().await;
            m.popup().expect("popup open").pane.clone()
        };

        let reply = one_shot(
            &registry,
            &ClientMsg::SendInput {
                session: Some("pop".into()),
                bytes: bytes::Bytes::from_static(b"popup_marker\n"),
            },
        )
        .await;
        assert!(expect_command_result(reply).0);

        // The marker lands in the POPUP pane's screen...
        wait_screen_contains(&popup_pane, "popup_marker").await;

        // ...and capture reads the popup, not the layout pane.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let reply = one_shot(
                &registry,
                &ClientMsg::CapturePane {
                    session: Some("pop".into()),
                },
            )
            .await;
            let text = match reply {
                ServerMsg::PaneCapture { text } => text,
                other => panic!("expected PaneCapture, got {other:?}"),
            };
            if text.contains("popup_marker") {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "popup capture missed the marker; got:\n{text}"
            );
            time::sleep(Duration::from_millis(20)).await;
        }

        // Kill the popup child so it doesn't outlive the test.
        session
            .handle_command(plexy_glass_mux::Command::ClosePopup)
            .await
            .unwrap();
    }

    // `CaptureLastCommand` with no completed block returns `CommandResult{ok:false}`
    // carrying the standard no-blocks message.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn capture_last_command_no_blocks_returns_error() {
        let _g = isolate();
        let registry = Arc::new(crate::SessionRegistry::new());
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        registry
            .attach_or_create("noblk".into(), script_cat(), script_size(), cfg)
            .await
            .unwrap();

        let reply = one_shot(
            &registry,
            &ClientMsg::CaptureLastCommand {
                session: Some("noblk".into()),
            },
        )
        .await;
        let (ok, message) = expect_command_result(reply);
        assert!(
            !ok,
            "CaptureLastCommand with no blocks must return ok:false"
        );
        let msg = message.expect("no-blocks path must carry a message");
        assert!(
            msg.contains("no command blocks"),
            "unexpected no-blocks text: {msg}"
        );
    }

    // `CaptureLastCommand` finds the last completed block and returns its
    // output text via `PaneCapture`. The block is planted directly in the
    // pane's screen (same pattern as `copy_output_pushes_buffer_and_sets_status`).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn capture_last_command_returns_block_text() {
        use plexy_glass_emulator::RowMark;
        let _g = isolate();
        let registry = Arc::new(crate::SessionRegistry::new());
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        let session = registry
            .attach_or_create("blk".into(), script_cat(), script_size(), cfg)
            .await
            .unwrap();

        // Plant a completed block in the pane's screen:
        // row 0: PROMPT_START ("$ echo hi")
        // row 1: OUTPUT_START ("out1")
        // row 2: PROMPT_START + BLOCK_END ("$ next"), the common D+A flow
        {
            let m = session.window_manager.lock().await;
            let pane = m.active_window().active_pane().unwrap();
            pane.with_screen_mut(|s| {
                write_grid_row(s, 0, "$ echo hi");
                s.active.rows[0].mark.set(RowMark::PROMPT_START);
                write_grid_row(s, 1, "out1");
                s.active.rows[1].mark.set(RowMark::OUTPUT_START);
                write_grid_row(s, 2, "$ next");
                s.active.rows[2].mark.set(RowMark::PROMPT_START);
                s.active.rows[2].mark.set(RowMark::BLOCK_END);
                s.active.rows[2].mark.set_exit(Some(0));
            });
        }

        let reply = one_shot(
            &registry,
            &ClientMsg::CaptureLastCommand {
                session: Some("blk".into()),
            },
        )
        .await;
        let text = match reply {
            ServerMsg::PaneCapture { text } => text,
            other => panic!("expected PaneCapture, got {other:?}"),
        };
        // The output range covers the OUTPUT_START row ("out1") through the
        // row before the next PROMPT_START, which is row 1. Row 2 belongs to
        // the NEXT block's prompt and must NOT appear in the capture.
        assert!(
            text.contains("out1"),
            "block text must include output: {text}"
        );
        assert!(
            !text.contains("$ next"),
            "next-prompt row must not appear in capture: {text}"
        );
        assert!(
            !text.contains("$ echo hi"),
            "prompt row must not appear (output range only): {text}"
        );
    }

    // `CaptureLastCommand` for a non-existent session returns `CommandResult{ok:false}`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn capture_last_command_unknown_session_returns_error() {
        let _g = isolate();
        let registry = Arc::new(crate::SessionRegistry::new());

        let reply = one_shot(
            &registry,
            &ClientMsg::CaptureLastCommand {
                session: Some("nosuchsession".into()),
            },
        )
        .await;
        let (ok, message) = expect_command_result(reply);
        assert!(!ok, "missing session must return ok:false");
        let msg = message.expect("session-miss must carry a message");
        assert!(
            msg.contains("no session"),
            "unexpected session-miss text: {msg}"
        );
    }

    // `CaptureLastBlock` returns the block's parts: output text, the closing
    // D's exit code, and the command line between the B and C marks. Seeded
    // over the wire through cat (the tty echo mangles ESC to ^[, so only
    // cat's verbatim copy sets marks; asserts are contains-style).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn capture_last_block_returns_structured_parts() {
        let _g = isolate();
        let registry = Arc::new(crate::SessionRegistry::new());
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        let session = registry
            .attach_or_create("lblk".into(), script_cat(), script_size(), cfg)
            .await
            .unwrap();

        // A "$ " B "demo_cmd" newline C "OUT_J3" newline D;4 A, the common
        // shell flow with a shared D+A row. Trailing \n so the canonical-mode
        // line discipline delivers the final partial line to cat.
        let reply = one_shot(
            &registry,
            &ClientMsg::SendInput {
                session: Some("lblk".into()),
                bytes: bytes::Bytes::from_static(
                    b"\x1b]133;A\x07$ \x1b]133;B\x07demo_cmd\r\n\x1b]133;C\x07OUT_J3\r\n\x1b]133;D;4\x07\x1b]133;A\x07\n",
                ),
            },
        )
        .await;
        assert!(expect_command_result(reply).0, "seeding the block failed");

        let pane = session
            .window_manager
            .lock()
            .await
            .input_target_pane()
            .expect("session has a pane")
            .clone();
        wait_screen_state(&pane, "seeded block completed with exit 4", |s| {
            blocks::last_completed_prompt(s).is_some_and(|p| blocks::closing_exit(s, p) == Some(4))
        })
        .await;

        let reply = one_shot(
            &registry,
            &ClientMsg::CaptureLastBlock {
                session: Some("lblk".into()),
            },
        )
        .await;
        let (text, exit, command_line) = match reply {
            ServerMsg::BlockCapture {
                text,
                exit,
                command_line,
            } => (text, exit, command_line),
            other => panic!("expected BlockCapture, got {other:?}"),
        };
        assert!(text.contains("OUT_J3"), "block output missing: {text:?}");
        assert_eq!(exit, Some(4));
        let cmd = command_line.expect("B and C marks present — command_line must be Some");
        assert!(cmd.contains("demo_cmd"), "command line missing: {cmd:?}");
    }

    // Without a `133;B` the command line is unextractable: `BlockCapture`
    // carries `command_line: None` while text/exit still work.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn capture_last_block_without_b_mark_has_no_command_line() {
        let _g = isolate();
        let registry = Arc::new(crate::SessionRegistry::new());
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        let session = registry
            .attach_or_create("noB".into(), script_cat(), script_size(), cfg)
            .await
            .unwrap();

        let reply = one_shot(
            &registry,
            &ClientMsg::SendInput {
                session: Some("noB".into()),
                bytes: bytes::Bytes::from_static(
                    b"\x1b]133;A\x07$ demo_cmd\r\n\x1b]133;C\x07OUT_J3\r\n\x1b]133;D;4\x07\x1b]133;A\x07\n",
                ),
            },
        )
        .await;
        assert!(expect_command_result(reply).0, "seeding the block failed");

        let pane = session
            .window_manager
            .lock()
            .await
            .input_target_pane()
            .expect("session has a pane")
            .clone();
        wait_screen_state(&pane, "seeded no-B block completed with exit 4", |s| {
            blocks::last_completed_prompt(s).is_some_and(|p| blocks::closing_exit(s, p) == Some(4))
        })
        .await;

        let reply = one_shot(
            &registry,
            &ClientMsg::CaptureLastBlock {
                session: Some("noB".into()),
            },
        )
        .await;
        let (text, exit, command_line) = match reply {
            ServerMsg::BlockCapture {
                text,
                exit,
                command_line,
            } => (text, exit, command_line),
            other => panic!("expected BlockCapture, got {other:?}"),
        };
        assert!(text.contains("OUT_J3"), "block output missing: {text:?}");
        assert_eq!(exit, Some(4));
        assert_eq!(command_line, None, "no 133;B — command_line must be None");
    }

    // `CaptureLastBlock` with no completed block returns `CommandResult{ok:false}`
    // carrying the standard no-blocks message (same asymmetry as the siblings).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn capture_last_block_no_blocks_returns_error() {
        let _g = isolate();
        let registry = Arc::new(crate::SessionRegistry::new());
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        registry
            .attach_or_create("noblk2".into(), script_cat(), script_size(), cfg)
            .await
            .unwrap();

        let reply = one_shot(
            &registry,
            &ClientMsg::CaptureLastBlock {
                session: Some("noblk2".into()),
            },
        )
        .await;
        let (ok, message) = expect_command_result(reply);
        assert!(!ok, "CaptureLastBlock with no blocks must return ok:false");
        let msg = message.expect("no-blocks path must carry a message");
        assert!(
            msg.contains("no command blocks"),
            "unexpected no-blocks text: {msg}"
        );
    }

    // `CaptureLastBlock` for a non-existent session returns `CommandResult{ok:false}`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn capture_last_block_unknown_session_returns_error() {
        let _g = isolate();
        let registry = Arc::new(crate::SessionRegistry::new());

        let reply = one_shot(
            &registry,
            &ClientMsg::CaptureLastBlock {
                session: Some("nosuchsession".into()),
            },
        )
        .await;
        let (ok, message) = expect_command_result(reply);
        assert!(!ok, "missing session must return ok:false");
        let msg = message.expect("session-miss must carry a message");
        assert!(
            msg.contains("no session"),
            "unexpected session-miss text: {msg}"
        );
    }

    // ── ExecCommand (CLI `run`) ──────────────────────────────────────────────
    //
    // All against a `cat` child: injected bytes are echoed back verbatim and
    // the emulator parses the embedded OSC 133 sequences as pane output. Note
    // that the tty's own echo mangles ESC into ^[ alongside cat's copy, so
    // screen asserts are contains-style, never exact.

    /// Poll `pane`'s screen until `pred` holds (e.g. "a prompt mark landed").
    async fn wait_screen_state<F>(pane: &Pane, desc: &str, pred: F)
    where
        F: Fn(&plexy_glass_emulator::Screen) -> bool,
    {
        use std::time::{Duration, Instant};
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if pane.with_screen(&pred) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "screen never reached state: {desc}"
            );
            time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Whether any PROMPT_START exists anywhere in the unified line space.
    fn has_prompt(s: &plexy_glass_emulator::Screen) -> bool {
        plexy_glass_mux::prev_prompt_line(s, u32::MAX).is_some()
    }

    /// Create a cat-backed session, seed a `133;A` prompt mark over the wire,
    /// and poll until the pane is observably at a prompt (the mark must be
    /// *processed*, not merely sent, because the tty echo races cat's verbatim copy).
    async fn exec_fixture(registry: &Arc<crate::SessionRegistry>, name: &str) -> Pane {
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        let session = registry
            .attach_or_create(name.into(), script_cat(), script_size(), cfg)
            .await
            .unwrap();
        let reply = one_shot(
            registry,
            &ClientMsg::SendInput {
                session: Some(name.into()),
                bytes: bytes::Bytes::from_static(b"\x1b]133;A\x07\n"),
            },
        )
        .await;
        assert!(
            expect_command_result(reply).0,
            "seeding the prompt mark failed"
        );
        let pane = session
            .window_manager
            .lock()
            .await
            .input_target_pane()
            .expect("session has a pane")
            .clone();
        wait_screen_state(&pane, "seeded prompt mark processed", |s| {
            has_prompt(s) && blocks::pane_at_prompt(s)
        })
        .await;
        pane
    }

    fn expect_exec_done(reply: ServerMsg) -> (Option<i32>, String, bool) {
        match reply {
            ServerMsg::ExecDone {
                exit,
                output,
                timed_out,
            } => (exit, output, timed_out),
            other => panic!("expected ExecDone, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn exec_command_happy_path() {
        let _g = isolate();
        let registry = Arc::new(crate::SessionRegistry::new());
        let _pane = exec_fixture(&registry, "exec1").await;

        // `cat` echoes the injected text; the emulator parses the embedded
        // C / output / D;3 / A sequence as a completing command block.
        let reply = one_shot(
            &registry,
            &ClientMsg::ExecCommand {
                session: Some("exec1".into()),
                text: "\x1b]133;C\x07EXEC_OUT_1\r\n\x1b]133;D;3\x07\x1b]133;A\x07".into(),
                timeout_ms: None,
            },
        )
        .await;
        let (exit, output, timed_out) = expect_exec_done(reply);
        assert_eq!(exit, Some(3));
        assert!(!timed_out);
        assert!(
            output.contains("EXEC_OUT_1"),
            "block output missing: {output:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn exec_command_exit_zero() {
        let _g = isolate();
        let registry = Arc::new(crate::SessionRegistry::new());
        let _pane = exec_fixture(&registry, "exec0").await;

        let reply = one_shot(
            &registry,
            &ClientMsg::ExecCommand {
                session: Some("exec0".into()),
                text: "\x1b]133;C\x07EXEC_OUT_OK\r\n\x1b]133;D;0\x07\x1b]133;A\x07".into(),
                timeout_ms: None,
            },
        )
        .await;
        let (exit, output, timed_out) = expect_exec_done(reply);
        assert_eq!(exit, Some(0));
        assert!(!timed_out);
        assert!(
            output.contains("EXEC_OUT_OK"),
            "block output missing: {output:?}"
        );
    }

    // A bare `133;D` (no exit payload) completes the wait with exit None,
    // NOT a timeout.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn exec_command_bare_d_reports_no_exit() {
        let _g = isolate();
        let registry = Arc::new(crate::SessionRegistry::new());
        let _pane = exec_fixture(&registry, "execnd").await;

        let reply = one_shot(
            &registry,
            &ClientMsg::ExecCommand {
                session: Some("execnd".into()),
                text: "\x1b]133;C\x07EXEC_OUT_ND\r\n\x1b]133;D\x07\x1b]133;A\x07".into(),
                timeout_ms: None,
            },
        )
        .await;
        let (exit, output, timed_out) = expect_exec_done(reply);
        assert_eq!(exit, None);
        assert!(!timed_out);
        assert!(
            output.contains("EXEC_OUT_ND"),
            "block output missing: {output:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn exec_command_no_marks_refused() {
        let _g = isolate();
        let registry = Arc::new(crate::SessionRegistry::new());
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        registry
            .attach_or_create("nomark".into(), script_cat(), script_size(), cfg)
            .await
            .unwrap();

        let reply = one_shot(
            &registry,
            &ClientMsg::ExecCommand {
                session: Some("nomark".into()),
                text: "echo hi".into(),
                timeout_ms: None,
            },
        )
        .await;
        let (ok, message) = expect_command_result(reply);
        assert!(!ok);
        assert_eq!(message.as_deref(), Some(NO_BLOCKS_MSG));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn exec_command_busy_refused() {
        let _g = isolate();
        let registry = Arc::new(crate::SessionRegistry::new());
        let pane = exec_fixture(&registry, "busy").await;

        // Open a block without closing it: A then C → mid-command.
        let reply = one_shot(
            &registry,
            &ClientMsg::SendInput {
                session: Some("busy".into()),
                bytes: bytes::Bytes::from_static(b"\x1b]133;C\x07\n"),
            },
        )
        .await;
        assert!(expect_command_result(reply).0);
        wait_screen_state(&pane, "C mark processed (pane busy)", |s| {
            has_prompt(s) && !blocks::pane_at_prompt(s)
        })
        .await;

        let reply = one_shot(
            &registry,
            &ClientMsg::ExecCommand {
                session: Some("busy".into()),
                text: "echo hi".into(),
                timeout_ms: None,
            },
        )
        .await;
        let (ok, message) = expect_command_result(reply);
        assert!(!ok);
        assert_eq!(
            message.as_deref(),
            Some("pane is busy: a command is running")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn exec_command_alt_screen_refused() {
        let _g = isolate();
        let registry = Arc::new(crate::SessionRegistry::new());
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        let session = registry
            .attach_or_create("altscr".into(), script_cat(), script_size(), cfg)
            .await
            .unwrap();

        // Enter the alt screen; poll the screen STATE (not text, the alt
        // screen shows nothing useful) until the switch is processed.
        let reply = one_shot(
            &registry,
            &ClientMsg::SendInput {
                session: Some("altscr".into()),
                bytes: bytes::Bytes::from_static(b"\x1b[?1049h\n"),
            },
        )
        .await;
        assert!(expect_command_result(reply).0);
        let pane = session
            .window_manager
            .lock()
            .await
            .input_target_pane()
            .expect("session has a pane")
            .clone();
        wait_screen_state(&pane, "alt screen active", |s| s.alt.is_some()).await;

        let reply = one_shot(
            &registry,
            &ClientMsg::ExecCommand {
                session: Some("altscr".into()),
                text: "echo hi".into(),
                timeout_ms: None,
            },
        )
        .await;
        let (ok, message) = expect_command_result(reply);
        assert!(!ok);
        assert_eq!(
            message.as_deref(),
            Some("pane is busy: alternate screen is active")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn exec_command_unknown_session_refused() {
        let _g = isolate();
        let registry = Arc::new(crate::SessionRegistry::new());

        let reply = one_shot(
            &registry,
            &ClientMsg::ExecCommand {
                session: Some("nope".into()),
                text: "echo hi".into(),
                timeout_ms: None,
            },
        )
        .await;
        let (ok, message) = expect_command_result(reply);
        assert!(!ok);
        assert!(message.unwrap_or_default().contains("no session \"nope\""));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn exec_command_timeout_is_structural() {
        let _g = isolate();
        let registry = Arc::new(crate::SessionRegistry::new());
        let _pane = exec_fixture(&registry, "exectmo").await;

        // The text never emits a D; the 50 ms timeout must fire.
        let reply = one_shot(
            &registry,
            &ClientMsg::ExecCommand {
                session: Some("exectmo".into()),
                text: "NO_COMPLETION_HERE".into(),
                timeout_ms: Some(50),
            },
        )
        .await;
        let (exit, output, timed_out) = expect_exec_done(reply);
        assert!(
            timed_out,
            "50 ms timeout with no D must be structural timed_out"
        );
        assert_eq!(exit, None);
        assert_eq!(output, "");
    }

    // The achievable stale-D guarantee (spec's fencing-honesty note): a `D`
    // PROCESSED before the baseline read never satisfies the wait. Seed a
    // fully completed block, poll it processed, then exec a never-completing
    // text, so the reply must be a timeout, never the stale block's exit 9.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn exec_command_stale_d_never_satisfies_wait() {
        let _g = isolate();
        let registry = Arc::new(crate::SessionRegistry::new());
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        let session = registry
            .attach_or_create("stale".into(), script_cat(), script_size(), cfg)
            .await
            .unwrap();

        // A, C, D;9, then the next prompt's A: a complete historical block.
        let reply = one_shot(
            &registry,
            &ClientMsg::SendInput {
                session: Some("stale".into()),
                bytes: bytes::Bytes::from_static(
                    b"\x1b]133;A\x07\n\x1b]133;C\x07stale_out\n\x1b]133;D;9\x07\x1b]133;A\x07\n",
                ),
            },
        )
        .await;
        assert!(expect_command_result(reply).0);
        let pane = session
            .window_manager
            .lock()
            .await
            .input_target_pane()
            .expect("session has a pane")
            .clone();
        wait_screen_state(&pane, "seeded block completed (D;9 processed)", |s| {
            s.blocks_completed >= 1 && blocks::pane_at_prompt(s)
        })
        .await;

        let reply = one_shot(
            &registry,
            &ClientMsg::ExecCommand {
                session: Some("stale".into()),
                text: "NEVER_COMPLETES".into(),
                timeout_ms: Some(100),
            },
        )
        .await;
        let (exit, _output, timed_out) = expect_exec_done(reply);
        assert!(
            timed_out,
            "the stale D;9 must not satisfy the new run's wait"
        );
        assert_ne!(exit, Some(9), "stale exit code leaked into the reply");
    }

    /// Open a connection, handshake, and send `msg` WITHOUT reading the reply,
    /// for tests that interfere with the wait (child kill, client drop).
    async fn start_exec(
        registry: &Arc<crate::SessionRegistry>,
        msg: &ClientMsg,
    ) -> (
        task::JoinHandle<Result<(), DaemonError>>,
        io::ReadHalf<io::DuplexStream>,
        io::WriteHalf<io::DuplexStream>,
    ) {
        let (server_side, client_side) = duplex(64 * 1024);
        let reg = Arc::clone(registry);
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        let server = tokio::spawn(async move { serve(server_side, 7, reg, cfg).await });
        let (mut cr, mut cw) = io::split(client_side);
        let _ = client_handshake(&mut cr, &mut cw).await.unwrap();
        Codec::write_frame(&mut cw, &postcard::to_allocvec(msg).unwrap())
            .await
            .unwrap();
        (server, cr, cw)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn exec_command_child_exit_mid_wait() {
        let _g = isolate();
        let registry = Arc::new(crate::SessionRegistry::new());
        let pane = exec_fixture(&registry, "execkill").await;

        let (server, mut cr, _cw) = start_exec(
            &registry,
            &ClientMsg::ExecCommand {
                session: Some("execkill".into()),
                text: "EXEC_HOLD_1".into(),
                timeout_ms: None,
            },
        )
        .await;
        // The marker on screen proves the daemon injected: preconditions
        // passed, the wait is (or is about to be) underway. Killing earlier
        // would still be correct (exit_rx is a watch), but this pins the
        // mid-wait shape.
        wait_screen_contains(&pane, "EXEC_HOLD_1").await;
        pane.kill_child();

        let frame = time::timeout(Duration::from_secs(5), Codec::read_frame(&mut cr))
            .await
            .expect("no reply after child exit")
            .unwrap()
            .expect("server closed without replying");
        let reply: ServerMsg = postcard::from_bytes(&frame).unwrap();
        let (ok, message) = expect_command_result(reply);
        assert!(!ok);
        assert_eq!(message.as_deref(), Some("run: pane child exited"));
        server.await.unwrap().unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn exec_command_client_drop_abandons_wait() {
        let _g = isolate();
        let registry = Arc::new(crate::SessionRegistry::new());
        let pane = exec_fixture(&registry, "execdrop").await;

        let (server, cr, cw) = start_exec(
            &registry,
            &ClientMsg::ExecCommand {
                session: Some("execdrop".into()),
                text: "EXEC_HOLD_2".into(),
                timeout_ms: None,
            },
        )
        .await;
        wait_screen_contains(&pane, "EXEC_HOLD_2").await;

        // Drop the client mid-wait: the serve task must observe EOF on its
        // reader and abandon the wait, no immortal 25 ms poll task left behind.
        drop(cr);
        drop(cw);
        let result = time::timeout(Duration::from_secs(5), server)
            .await
            .expect("serve task never completed after client drop");
        result.unwrap().unwrap();
    }

    // A mid-command RIS (`\x1bc`) echoed back by cat causes the emulator to
    // rebuild Screen::new, resetting blocks_completed to 0. With baseline=1
    // (one completed block seeded beforehand), 0 < 1 → ExecTick::Reset →
    // CommandResult { ok: false, "run: pane was reset mid-command" }.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn exec_command_reset_mid_command_refused() {
        let _g = isolate();
        let registry = Arc::new(crate::SessionRegistry::new());
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        let session = registry
            .attach_or_create("execris".into(), script_cat(), script_size(), cfg)
            .await
            .unwrap();

        // Seed a FULLY completed block: A, C, D;0, A.
        // Counter becomes 1; pane lands at a fresh prompt.
        let reply = one_shot(
            &registry,
            &ClientMsg::SendInput {
                session: Some("execris".into()),
                bytes: bytes::Bytes::from_static(
                    b"\x1b]133;A\x07\n\x1b]133;C\x07ris_seed\n\x1b]133;D;0\x07\x1b]133;A\x07\n",
                ),
            },
        )
        .await;
        assert!(
            expect_command_result(reply).0,
            "seeding completed block failed"
        );
        let pane = session
            .window_manager
            .lock()
            .await
            .input_target_pane()
            .expect("session has a pane")
            .clone();
        // Poll until the counter is >= 1 AND the pane is at a prompt (D;0 and the
        // second A are processed).
        wait_screen_state(&pane, "completed block + at-prompt processed", |s| {
            s.blocks_completed >= 1 && blocks::pane_at_prompt(s)
        })
        .await;

        // Start exec with a non-completing text; the wait is underway.
        let (server, mut cr, _cw) = start_exec(
            &registry,
            &ClientMsg::ExecCommand {
                session: Some("execris".into()),
                text: "EXEC_RIS_HOLD".into(),
                timeout_ms: None,
            },
        )
        .await;
        // Wait until the injected text is visible (preconditions passed and
        // the `ExecCommand`'s input was written to the pane).
        wait_screen_contains(&pane, "EXEC_RIS_HOLD").await;

        // Send RIS via a second connection: cat echoes \x1bc back; the
        // emulator processes ESC c as RIS and rebuilds Screen::new →
        // blocks_completed resets to 0 < baseline (1).
        let ris_reply = one_shot(
            &registry,
            &ClientMsg::SendInput {
                session: Some("execris".into()),
                bytes: bytes::Bytes::from_static(b"\x1bc\n"),
            },
        )
        .await;
        assert!(expect_command_result(ris_reply).0, "RIS SendInput failed");

        // The exec serve task must observe the reset and reply with a refusal.
        let frame = time::timeout(Duration::from_secs(5), Codec::read_frame(&mut cr))
            .await
            .expect("no reply after RIS reset")
            .unwrap()
            .expect("server closed without replying");
        let reply: ServerMsg = postcard::from_bytes(&frame).unwrap();
        let (ok, message) = expect_command_result(reply);
        assert!(!ok);
        assert_eq!(message.as_deref(), Some("run: pane was reset mid-command"));
        server.await.unwrap().unwrap();
    }

    // A second ClientMsg on the same connection while `serve_exec` is waiting
    // must produce CommandResult { ok: false, "run: unexpected message during
    // wait" }, since the connection is exclusively serving a single `run`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn exec_command_unexpected_frame_refused() {
        let _g = isolate();
        let registry = Arc::new(crate::SessionRegistry::new());
        let pane = exec_fixture(&registry, "execuf").await;

        let (server, mut cr, mut cw) = start_exec(
            &registry,
            &ClientMsg::ExecCommand {
                session: Some("execuf".into()),
                text: "EXEC_UF_HOLD".into(),
                timeout_ms: None,
            },
        )
        .await;
        // Wait until the injected text is visible so the exec wait is underway.
        wait_screen_contains(&pane, "EXEC_UF_HOLD").await;

        // Write a second frame on the same connection (any `ClientMsg` will do).
        Codec::write_frame(
            &mut cw,
            &postcard::to_allocvec(&ClientMsg::ListSessions).unwrap(),
        )
        .await
        .unwrap();

        let frame = time::timeout(Duration::from_secs(5), Codec::read_frame(&mut cr))
            .await
            .expect("no reply after unexpected frame")
            .unwrap()
            .expect("server closed without replying");
        let reply: ServerMsg = postcard::from_bytes(&frame).unwrap();
        let (ok, message) = expect_command_result(reply);
        assert!(!ok);
        assert_eq!(
            message.as_deref(),
            Some("run: unexpected message during wait")
        );
        server.await.unwrap().unwrap();
    }
}
