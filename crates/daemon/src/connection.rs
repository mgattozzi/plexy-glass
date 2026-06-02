//! One connection from a client.

use crate::{
    InputEvent, InputRouter, error::DaemonError, input_router::decode_protocol,
    registry::SessionRegistry, renderer::Renderer, session::Session,
};
use plexy_glass_mux::{Command, KeymapAction, PromptCommand, VirtualScreen};
use plexy_glass_protocol::{
    ClientMsg, Codec, ProtocolError, PtySize, ServerMsg, SpawnSpec, server_handshake,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, watch};

pub struct Connection;

impl Connection {
    pub async fn serve<S>(
        stream: S,
        daemon_pid: u32,
        registry: Arc<SessionRegistry>,
        config: Arc<plexy_glass_config::Config>,
    ) -> Result<(), DaemonError>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (mut reader, mut writer) = tokio::io::split(stream);
        let client_hello = server_handshake(&mut reader, &mut writer, daemon_pid).await?;

        let frame = Codec::read_frame(&mut reader).await?.ok_or_else(|| {
            DaemonError::Io(std::io::Error::other("client closed before first message"))
        })?;
        let msg: ClientMsg = postcard::from_bytes(&frame)
            .map_err(|e| plexy_glass_protocol::errors::CodecError::Decode(e.to_string()))?;

        match msg {
            ClientMsg::ListSessions => {
                let entries = registry.list().await;
                send_msg(&mut writer, &ServerMsg::SessionList { entries }).await?;
                Ok(())
            }
            ClientMsg::ListSavedSessions => {
                let entries = crate::persist::list_saved()
                    .into_iter()
                    .map(|(name, windows, panes)| plexy_glass_protocol::SavedSessionEntry {
                        name,
                        windows,
                        panes,
                    })
                    .collect();
                send_msg(&mut writer, &ServerMsg::SavedSessionList { entries }).await?;
                Ok(())
            }
            ClientMsg::KillSession { name } => match registry.kill(&name).await {
                Ok(()) => send_msg(&mut writer, &ServerMsg::SessionKilled { name }).await,
                Err(DaemonError::Protocol(perr)) => {
                    send_msg(&mut writer, &ServerMsg::Error(perr)).await
                }
                Err(e) => Err(e),
            },
            ClientMsg::AttachOrCreate { name, create_if_missing, cmd, size } => {
                serve_attach(
                    reader, writer, registry, name, create_if_missing, cmd, size, config,
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
}

async fn send_msg<W>(writer: &mut W, msg: &ServerMsg) -> Result<(), DaemonError>
where
    W: AsyncWrite + Unpin,
{
    let bytes = postcard::to_allocvec(msg)
        .map_err(|e| plexy_glass_protocol::errors::CodecError::Encode(e.to_string()))?;
    Codec::write_frame(writer, &bytes).await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)] // one internal entry point, splitting it up would lose clarity
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
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    // Per-connection decode context from the handshake. `kbd` scopes THIS
    // client's key decode (deterministic, replacing the Permissive default).
    // `term` is informational only: XTGETTCAP TN comes from the pane's own
    // `$TERM` at spawn, never a per-client value (multi-client), so it stays
    // unused here.
    let _client_term = client_hello.term;
    let client_kbd = client_hello.kbd;

    // Resolve or create the session. `session` is reassigned in place by
    // `switch_session` when the client switches to another session.
    let mut session = match name {
        Some(n) => match registry.get(&n).await {
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
        },
        None => {
            // Smart default fallback: 0 -> create "main"; 1 -> attach to it; 2+ -> ambiguous.
            let entries = registry.list().await;
            match entries.len() {
                0 => {
                    let spec = cmd.unwrap_or_else(default_spawn_spec);
                    let cfg = Arc::clone(&config);
                    // `attach_or_create` restores "main" from disk if saved.
                    match registry.attach_or_create("main".into(), spec, size, cfg).await {
                        Ok(s) => s,
                        Err(DaemonError::Protocol(perr)) => {
                            return send_msg(&mut writer, &ServerMsg::Error(perr)).await;
                        }
                        Err(e) => return Err(e),
                    }
                }
                1 => match registry.get(&entries[0].name).await {
                    Some(s) => s,
                    None => {
                        // We raced with a kill, so surface it as session-not-found.
                        return send_msg(
                            &mut writer,
                            &ServerMsg::Error(ProtocolError::SessionNotFound {
                                name: entries[0].name.clone(),
                            }),
                        )
                        .await;
                    }
                },
                n => {
                    let count = u8::try_from(n).unwrap_or(u8::MAX);
                    return send_msg(
                        &mut writer,
                        &ServerMsg::Error(ProtocolError::AmbiguousSession { count }),
                    )
                    .await;
                }
            }
        }
    };

    // Register this connection as a client. `register_client` calls
    // `blocking_lock` internally, so dispatch it off the async runtime.
    let session_for_register = Arc::clone(&session);
    let handle = match tokio::task::spawn_blocking(move || {
        session_for_register.register_client(size)
    })
    .await
    {
        Ok(Ok(h)) => h,
        Ok(Err(DaemonError::Protocol(perr))) => {
            return send_msg(&mut writer, &ServerMsg::Error(perr)).await;
        }
        Ok(Err(e)) => return Err(e),
        Err(join) => return Err(DaemonError::Io(std::io::Error::other(join.to_string()))),
    };

    let mut client_id = handle.client_id;
    let session_name = session.name.clone();

    send_msg(
        &mut writer,
        &ServerMsg::Attached { session_name, client_id },
    )
    .await?;

    // Spawn the per-Connection renderer task. It owns the writer half from
    // here on out. `switch_tx` lets the input loop re-point the renderer at a
    // different session's frame stream (session switch) without reclaiming the
    // writer.
    let frame_rx = handle.frame_rx.clone();
    let (switch_tx, switch_rx) = mpsc::unbounded_channel::<watch::Receiver<Arc<VirtualScreen>>>();
    let renderer = Renderer::new();
    let mut renderer_task = tokio::spawn(async move {
        let _ = renderer.run(frame_rx, switch_rx, writer).await;
    });

    // Input loop. Scope key decode to the client's negotiated outer-terminal
    // protocol (older/unknown peers downgraded to Legacy upstream).
    let mut router = InputRouter::with_protocol(decode_protocol(client_kbd));
    let mut keymap = plexy_glass_keys::build_keymap(&config.keymap);
    let prefix_active = Arc::new(AtomicBool::new(false));

    loop {
        let frame = tokio::select! {
            biased;
            // Renderer exits when its `frame_rx` is closed, i.e. the session's
            // coordinator dropped its `frame_tx`. That means the session ended
            // (last pane exited, or the session was killed). Tear down so the
            // client process exits and `HostTty::restore` runs.
            _ = &mut renderer_task => break,
            result = Codec::read_frame(&mut reader) => match result {
                Ok(Some(f)) => f,
                Ok(None) => break,
                Err(_) => break,
            },
        };
        let msg: ClientMsg = match postcard::from_bytes(&frame) {
            Ok(m) => m,
            Err(_) => continue,
        };
        match msg {
            ClientMsg::Input(bytes) => {
                let events = router.classify(bytes.as_ref());
                let mut detach_requested = false;
                for event in events {
                    match event {
                        InputEvent::Mouse(me) => {
                            let _ = session.handle_mouse(me).await;
                            // Status-bar Detach click sets WindowManager.detach_requested.
                            // Propagate it to the local flag so this connection exits.
                            let mut mgr = session.window_manager.lock().await;
                            if mgr.detach_requested {
                                mgr.detach_requested = false;
                                detach_requested = true;
                            }
                        }
                        InputEvent::Key(ke, raw_bytes) => {
                            // An open overlay (rename / help) captures every key
                            // before the keymap or the shell, the same routing
                            // as copy mode below. The overlay was opened by a
                            // Command, so the opening keystroke already went
                            // through the keymap; every subsequent key lands
                            // here until commit/cancel.
                            let overlay_active = {
                                let m = session.window_manager.lock().await;
                                m.overlay().is_some()
                            };
                            if overlay_active {
                                let result = {
                                    let mut m = session.window_manager.lock().await;
                                    m.handle_overlay_key(&ke)
                                };
                                match result {
                                    crate::window_manager::OverlayKeyResult::Ignored => {}
                                    crate::window_manager::OverlayKeyResult::Redraw => {
                                        session.notify.notify_one();
                                    }
                                    crate::window_manager::OverlayKeyResult::Committed => {
                                        // A rename changed persistent state: redraw
                                        // and schedule a debounced save.
                                        session.notify.notify_one();
                                        session.mark_dirty();
                                    }
                                    crate::window_manager::OverlayKeyResult::SwitchSession(name) => {
                                        switch_session(
                                            &mut session,
                                            &mut client_id,
                                            size,
                                            &registry,
                                            &switch_tx,
                                            name,
                                        )
                                        .await;
                                    }
                                    crate::window_manager::OverlayKeyResult::Tree(action) => {
                                        dispatch_tree_action(
                                            &mut session,
                                            &mut client_id,
                                            size,
                                            &registry,
                                            &switch_tx,
                                            action,
                                        )
                                        .await;
                                    }
                                    crate::window_manager::OverlayKeyResult::Buffer(action) => {
                                        use plexy_glass_mux::BufferAction;
                                        match action {
                                            BufferAction::Paste(name) => {
                                                if let Some(content) =
                                                    registry.paste_buffer_get(&name).await
                                                {
                                                    paste_bytes(&session, content).await;
                                                } else {
                                                    // The overlay closed; repaint
                                                    // it away even on a get() miss.
                                                    session.notify.notify_one();
                                                }
                                            }
                                            BufferAction::Delete(name) => {
                                                registry.delete_paste_buffer(&name).await;
                                                // Repaint the still-open overlay.
                                                session.notify.notify_one();
                                            }
                                        }
                                    }
                                    crate::window_manager::OverlayKeyResult::Command(line) => {
                                        match plexy_glass_mux::command_prompt::parse(&line) {
                                            Err(e) => {
                                                session
                                                    .set_status_message(e.to_string())
                                                    .await;
                                            }
                                            Ok(PromptCommand::Detach) => {
                                                detach_requested = true;
                                            }
                                            Ok(PromptCommand::Reload) => {
                                                let _ = registry.reload_config().await;
                                                let new_cfg = session.config_snapshot();
                                                keymap = plexy_glass_keys::build_keymap(
                                                    &new_cfg.keymap,
                                                );
                                                session.notify.notify_one();
                                            }
                                            Ok(PromptCommand::Switch(name)) => {
                                                switch_session(
                                                    &mut session,
                                                    &mut client_id,
                                                    size,
                                                    &registry,
                                                    &switch_tx,
                                                    name,
                                                )
                                                .await;
                                            }
                                            Ok(PromptCommand::ChooseSession) => {
                                                open_session_picker_overlay(&session, &registry)
                                                    .await;
                                            }
                                            Ok(PromptCommand::ChooseTree) => {
                                                open_tree_overlay(&session, &registry).await;
                                            }
                                            Ok(PromptCommand::PasteBuffer) => {
                                                paste_top_buffer(&session, &registry).await;
                                            }
                                            Ok(PromptCommand::ChooseBuffer) => {
                                                open_buffer_picker_overlay(&session, &registry)
                                                    .await;
                                            }
                                            Ok(other) => {
                                                match session
                                                    .handle_prompt_command(other)
                                                    .await
                                                {
                                                    Ok(Some(msg)) => {
                                                        session.set_status_message(msg).await;
                                                    }
                                                    Ok(None) => {}
                                                    Err(e) => {
                                                        session
                                                            .set_status_message(e.to_string())
                                                            .await;
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                if detach_requested {
                                    break;
                                }
                                continue;
                            }
                            // Snap scrollback to live on any keystroke.
                            {
                                let manager = session.window_manager.lock().await;
                                if let Some(p) = manager.active_window().active_pane() {
                                    p.reset_scroll();
                                }
                            }
                            let action = keymap.consume(ke, raw_bytes);
                            prefix_active.store(keymap.prefix_active(), Ordering::SeqCst);
                            match action {
                                KeymapAction::PassThrough(event_ke, bytes_back) => {
                                    // If the active pane is in copy mode, route the key event
                                    // to the CopyModeHandler instead of the shell.
                                    let active_in_copy_mode = {
                                        let m = session.window_manager.lock().await;
                                        m.active_window()
                                            .active_pane()
                                            .map(|p| p.is_in_copy_mode())
                                            .unwrap_or(false)
                                    };
                                    if active_in_copy_mode {
                                        let action = {
                                            let m = session.window_manager.lock().await;
                                            let pane_opt = m.active_window().active_pane();
                                            pane_opt.and_then(|p| {
                                                let screen = p.with_screen(|s| s.clone());
                                                p.with_copy_mode_mut(|state| {
                                                    plexy_glass_mux::CopyModeHandler::handle(
                                                        &event_ke,
                                                        state,
                                                        &screen,
                                                    )
                                                })
                                            })
                                        };
                                        match action {
                                            Some(plexy_glass_mux::CopyModeAction::Render) => {
                                                session.notify.notify_one();
                                            }
                                            Some(plexy_glass_mux::CopyModeAction::Exit) => {
                                                let m = session.window_manager.lock().await;
                                                if let Some(p) = m.active_window().active_pane() {
                                                    p.exit_copy_mode();
                                                }
                                                session.notify.notify_one();
                                            }
                                            Some(plexy_glass_mux::CopyModeAction::Yank(text)) => {
                                                let _ = crate::osc_actions::write_clipboard(
                                                    text.as_bytes(),
                                                )
                                                .await;
                                                // Also push a paste buffer (before
                                                // re-taking the WM lock, so the
                                                // registry await isn't held under it).
                                                registry
                                                    .push_paste_buffer(text.into_bytes())
                                                    .await;
                                                let m = session.window_manager.lock().await;
                                                if let Some(p) = m.active_window().active_pane() {
                                                    p.exit_copy_mode();
                                                }
                                                session.notify.notify_one();
                                            }
                                            None => {}
                                        }
                                    } else {
                                        let _ = session
                                            .handle_key_event(&event_ke, &bytes_back)
                                            .await;
                                    }
                                }
                                KeymapAction::Command(cmd) => match cmd {
                                    Command::Detach => {
                                        detach_requested = true;
                                        break;
                                    }
                                    Command::ReloadConfig => {
                                        let _ = registry.reload_config().await;
                                        // Rebuild this Connection's keymap from
                                        // the new config so the user who fired
                                        // the reload sees binding changes
                                        // immediately.
                                        let new_cfg = session.config_snapshot();
                                        keymap = plexy_glass_keys::build_keymap(
                                            &new_cfg.keymap,
                                        );
                                        session.notify.notify_one();
                                    }
                                    Command::CommandPrompt => {
                                        // Opened here (not in handle_command)
                                        // because it needs the live session list
                                        // for `switch ` Tab-completion.
                                        let names: Vec<String> = registry
                                            .list()
                                            .await
                                            .into_iter()
                                            .map(|e| e.name)
                                            .collect();
                                        {
                                            let mut m = session.window_manager.lock().await;
                                            m.open_command_prompt(names);
                                        }
                                        session.notify.notify_one();
                                    }
                                    Command::ChooseSession => {
                                        open_session_picker_overlay(&session, &registry).await;
                                    }
                                    Command::ChooseTree => {
                                        open_tree_overlay(&session, &registry).await;
                                    }
                                    Command::PasteBuffer => {
                                        paste_top_buffer(&session, &registry).await;
                                    }
                                    Command::ChooseBuffer => {
                                        open_buffer_picker_overlay(&session, &registry).await;
                                    }
                                    other => {
                                        let _ = session.handle_command(other).await;
                                    }
                                },
                                KeymapAction::Pending => {
                                    session.notify.notify_one();
                                }
                                KeymapAction::Cancel => {
                                    session.notify.notify_one();
                                }
                            }
                        }
                        InputEvent::Paste(bytes) => {
                            let want_bracketed = {
                                let manager = session.window_manager.lock().await;
                                manager
                                    .active_window()
                                    .active_pane()
                                    .map(|p| p.with_screen(|s| {
                                        s.modes.contains(plexy_glass_emulator::Modes::BRACKETED_PASTE)
                                    }))
                                    .unwrap_or(false)
                            };
                            let payload = if want_bracketed {
                                wrap_paste(&bytes)
                            } else {
                                bytes
                            };
                            let _ = session.handle_input_bytes(&payload).await;
                        }
                        InputEvent::Bytes(bs) => {
                            let _ = session.handle_input_bytes(&bs).await;
                        }
                    }
                }
                if detach_requested {
                    break;
                }
            }
            ClientMsg::Resize(new_size) => {
                // Track the client's size so a later session switch can register
                // on the target session at the correct dimensions.
                size = new_size;
                let session_for_resize = Arc::clone(&session);
                let cid = client_id;
                let _ = tokio::task::spawn_blocking(move || {
                    session_for_resize.handle_resize(cid, new_size);
                })
                .await;
            }
            ClientMsg::Detach => break,
            ClientMsg::Shutdown => break,
            // Outer-terminal focus + color-scheme events. Decoded now; routing to
            // the focused/subscribing panes is wired in Task 15. Explicit arms (vs
            // the `_` catch-all) so that work has a marked starting point.
            ClientMsg::FocusIn | ClientMsg::FocusOut | ClientMsg::ColorScheme(_) => {}
            _ => {}
        }
    }
    cleanup_and_exit(session, client_id, renderer_task).await
}

async fn cleanup_and_exit(
    session: Arc<Session>,
    client_id: u64,
    renderer_task: tokio::task::JoinHandle<()>,
) -> Result<(), DaemonError> {
    let session_for_dereg = Arc::clone(&session);
    let _ = tokio::task::spawn_blocking(move || {
        session_for_dereg.deregister_client(client_id);
    })
    .await;
    renderer_task.abort();
    Ok(())
}

/// Re-point a live client at another running session in place. Registers on the
/// target *before* deregistering the source so the client is never momentarily
/// unattached, hands the renderer the target's frame stream (forcing a full
/// repaint), then swaps the loop's `session`/`client_id`. All failure paths land
/// on the transient status line and leave the client on the source session.
///
/// Returns `true` iff the client actually moved to a different session. Callers
/// that follow a switch with target-scoped work (e.g. focusing a window/pane by
/// id) MUST gate that work on `true`, because on failure `session` still points
/// at the source, and because pane/window ids are not unique across sessions,
/// applying the target's ids to the source would silently mutate the wrong
/// session.
async fn switch_session(
    session: &mut Arc<Session>,
    client_id: &mut u64,
    size: PtySize,
    registry: &Arc<SessionRegistry>,
    switch_tx: &mpsc::UnboundedSender<watch::Receiver<Arc<VirtualScreen>>>,
    name: String,
) -> bool {
    let Some(target) = registry.get(&name).await else {
        session.set_status_message(format!("no session: {name}")).await;
        return false;
    };
    if target.name == session.name {
        session.set_status_message(format!("already on {name}")).await;
        return false;
    }
    // `register_client` takes a `blocking_lock` internally, so keep it off the runtime.
    let target_for_register = Arc::clone(&target);
    let new_handle = match tokio::task::spawn_blocking(move || {
        target_for_register.register_client(size)
    })
    .await
    {
        Ok(Ok(h)) => h,
        _ => {
            session
                .set_status_message(format!("cannot switch to {name}"))
                .await;
            return false;
        }
    };
    // Re-point the renderer (rebind + invalidate + full repaint).
    let _ = switch_tx.send(new_handle.frame_rx.clone());
    let old = std::mem::replace(session, target);
    let old_id = std::mem::replace(client_id, new_handle.client_id);
    let _ = tokio::task::spawn_blocking(move || old.deregister_client(old_id)).await;
    session.set_status_message(format!("switched to {name}")).await;
    true
}

/// Snapshot the live sessions (sorted by name, current one marked) and open the
/// session picker. Shared by the `Ctrl+a w` keymap arm and the `:sessions`
/// command-prompt verb.
async fn open_session_picker_overlay(session: &Arc<Session>, registry: &Arc<SessionRegistry>) {
    let current = session.name.clone();
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
            plexy_glass_mux::PickerEntry { name: e.name, label, is_current }
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
    let current = session.name.clone();
    let mut snaps: Vec<crate::session::SessionTree> = Vec::new();
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
fn build_tree_nodes(
    snaps: &[crate::session::SessionTree],
    current: &str,
) -> Vec<plexy_glass_mux::TreeNode> {
    use plexy_glass_mux::{TreeNode, pane_label, window_label};
    let mut nodes: Vec<TreeNode> = Vec::new();
    for st in snaps {
        let is_cur = st.name == current;
        nodes.push(TreeNode {
            session: st.name.clone(),
            window: None,
            pane: None,
            depth: 0,
            label: format!(
                "{} \u{2014} {} win, {} panes",
                st.name,
                st.windows.len(),
                st.total_panes
            ),
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

/// Perform a choose-tree action. `Switch` re-points this client (and focuses the
/// chosen window/pane); the `Kill*`/`Rename*` actions reach into the target
/// session via the registry. The current session is always notified afterward so
/// its still-open overlay repaints the optimistic model update.
async fn dispatch_tree_action(
    session: &mut Arc<Session>,
    client_id: &mut u64,
    size: PtySize,
    registry: &Arc<SessionRegistry>,
    switch_tx: &mpsc::UnboundedSender<watch::Receiver<Arc<VirtualScreen>>>,
    action: plexy_glass_mux::TreeAction,
) {
    use plexy_glass_mux::TreeAction;
    match action {
        TreeAction::Switch { session: tgt, window, pane } => {
            // Only focus the chosen window/pane when the client is actually on
            // the target session: either it was already there, or the switch
            // succeeded. On a failed switch the client stays on the SOURCE, and
            // because ids are not unique across sessions, applying the target's
            // ids here would mutate the wrong session.
            let on_target = if tgt == session.name {
                true
            } else {
                switch_session(session, client_id, size, registry, switch_tx, tgt).await
            };
            if on_target {
                let mut m = session.window_manager.lock().await;
                if let Some(w) = window {
                    m.select_window_by_id(w);
                }
                if let Some(p) = pane {
                    m.focus_pane_by_id(p);
                }
            }
            session.notify.notify_one();
        }
        TreeAction::KillSession(name) => {
            match registry.kill(&name).await {
                Ok(()) => session.set_status_message(format!("killed {name}")).await,
                Err(e) => session.set_status_message(e.to_string()).await,
            }
            session.notify.notify_one();
        }
        TreeAction::KillWindow { session: tgt, window } => {
            if let Some(t) = registry.get(&tgt).await {
                {
                    let mut m = t.window_manager.lock().await;
                    m.kill_window_panes(window);
                }
                t.notify.notify_one();
            } else {
                session.set_status_message(format!("no session: {tgt}")).await;
            }
            session.notify.notify_one();
        }
        TreeAction::KillPane { session: tgt, pane } => {
            if let Some(t) = registry.get(&tgt).await {
                {
                    let mut m = t.window_manager.lock().await;
                    m.kill_pane_child(pane);
                }
                t.notify.notify_one();
            } else {
                session.set_status_message(format!("no session: {tgt}")).await;
            }
            session.notify.notify_one();
        }
        TreeAction::RenameWindow { session: tgt, window, name } => {
            if let Some(t) = registry.get(&tgt).await {
                {
                    let mut m = t.window_manager.lock().await;
                    m.rename_window_by_id(window, name);
                }
                t.mark_dirty();
                t.notify.notify_one();
            } else {
                session.set_status_message(format!("no session: {tgt}")).await;
            }
            session.notify.notify_one();
        }
        TreeAction::RenamePane { session: tgt, pane, name } => {
            if let Some(t) = registry.get(&tgt).await {
                {
                    let mut m = t.window_manager.lock().await;
                    m.rename_pane_by_id(pane, name);
                }
                t.mark_dirty();
                t.notify.notify_one();
            } else {
                session.set_status_message(format!("no session: {tgt}")).await;
            }
            session.notify.notify_one();
        }
    }
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

/// Paste the most-recent paste buffer into the active pane (bracketed if the
/// pane requests it), or set a status when there is none. Shared by `Ctrl+a ]`
/// and the `:paste` verb.
async fn paste_top_buffer(session: &Arc<Session>, registry: &Arc<SessionRegistry>) {
    match registry.paste_buffer_top().await {
        Some(content) => paste_bytes(session, content).await,
        None => session.set_status_message("no paste buffer".into()).await,
    }
}

/// Send `content` to the active pane, wrapping in bracketed-paste markers when
/// the pane has that mode on (mirrors `InputEvent::Paste`). Also used by the
/// choose-buffer overlay's paste action.
async fn paste_bytes(session: &Arc<Session>, content: Vec<u8>) {
    let want_bracketed = {
        let manager = session.window_manager.lock().await;
        manager
            .active_window()
            .active_pane()
            .map(|p| {
                p.with_screen(|s| s.modes.contains(plexy_glass_emulator::Modes::BRACKETED_PASTE))
            })
            .unwrap_or(false)
    };
    let payload = if want_bracketed { wrap_paste(&content) } else { content };
    let _ = session.handle_input_bytes(&payload).await;
}

fn wrap_paste(inner: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(inner.len() + 12);
    out.extend_from_slice(b"\x1b[200~");
    out.extend_from_slice(inner);
    out.extend_from_slice(b"\x1b[201~");
    out
}

fn default_spawn_spec() -> SpawnSpec {
    let program = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    SpawnSpec {
        program,
        args: vec![],
        env: vec![],
        cwd: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plexy_glass_protocol::{PROTOCOL_VERSION, PtySize, SpawnSpec, client_handshake};
    use tokio::io::duplex;

    #[test]
    fn wrap_paste_wraps_with_bracketed_paste_escapes() {
        let inner = b"hello world";
        let wrapped = wrap_paste(inner);
        assert_eq!(wrapped.as_slice(), b"\x1b[200~hello world\x1b[201~");
    }

    #[test]
    fn wrap_paste_empty_input() {
        let wrapped = wrap_paste(b"");
        assert_eq!(wrapped.as_slice(), b"\x1b[200~\x1b[201~");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn end_to_end_attach_renders_then_exits() {
        let (server_side, client_side) = duplex(64 * 1024);
        let server = tokio::spawn(async move {
            Connection::serve(
                server_side,
                7,
                Arc::new(crate::SessionRegistry::new()),
                Arc::new(plexy_glass_config::built_in_default()),
            )
            .await
        });

        let (mut cr, mut cw) = tokio::io::split(client_side);
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
            size: PtySize { rows: 8, cols: 24, pixel_width: 0, pixel_height: 0 },
        };
        let bytes = postcard::to_allocvec(&attach).unwrap();
        Codec::write_frame(&mut cw, &bytes).await.unwrap();

        let mut saw_attached = false;
        let mut saw_output = false;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        while tokio::time::Instant::now() < deadline {
            let frame = match tokio::time::timeout(
                std::time::Duration::from_millis(500),
                Codec::read_frame(&mut cr),
            )
            .await
            {
                Ok(Ok(Some(f))) => f,
                _ => break,
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
        use std::time::{Duration, Instant};
        use tokio::io::{AsyncReadExt, split};

        let registry = Arc::new(crate::SessionRegistry::new());
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        let size = PtySize { rows: 8, cols: 24, pixel_width: 0, pixel_height: 0 };
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
        let server =
            tokio::spawn(async move { Connection::serve(server_side, 7, reg, cfg2).await });

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
                    if Instant::now() > deadline {
                        panic!(
                            "timed out: a={:?} b={:?} (want a={want_a} b={want_b})",
                            clients_of(&entries, "a"),
                            clients_of(&entries, "b")
                        );
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
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

    // `Ctrl+a w` opens the picker; typing `b` filters to session "b"; Enter
    // switches there. Exercises the picker open → filter → commit → switch path.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_picker_filters_and_switches() {
        use std::time::{Duration, Instant};
        use tokio::io::{AsyncReadExt, split};

        let registry = Arc::new(crate::SessionRegistry::new());
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        let size = PtySize { rows: 10, cols: 40, pixel_width: 0, pixel_height: 0 };
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
        let server =
            tokio::spawn(async move { Connection::serve(server_side, 7, reg, cfg2).await });

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
                    if Instant::now() > deadline {
                        panic!(
                            "timed out: alpha={:?} beta={:?}",
                            clients_of(&entries, "alpha"),
                            clients_of(&entries, "beta")
                        );
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
        };
        wait_until(1, 0).await;

        // Ctrl+a w (0x01 'w') opens the picker; "b" filters to "beta"; Enter (0x0d).
        let input = ClientMsg::Input(bytes::Bytes::from_static(b"\x01wb\r"));
        Codec::write_frame(&mut cw, &postcard::to_allocvec(&input).unwrap())
            .await
            .unwrap();

        wait_until(0, 1).await;
        server.abort();
    }

    // `Ctrl+a W` opens the choose-tree; the tree lists every session
    // (sorted: alpha then beta, each session+window+pane = 3 rows). Three `j`
    // moves the selection to the "beta" session node; Enter switches there.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn choose_tree_switches_sessions() {
        use std::time::{Duration, Instant};
        use tokio::io::{AsyncReadExt, split};

        let registry = Arc::new(crate::SessionRegistry::new());
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        let size = PtySize { rows: 16, cols: 60, pixel_width: 0, pixel_height: 0 };
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
        let server =
            tokio::spawn(async move { Connection::serve(server_side, 7, reg, cfg2).await });

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
                    if Instant::now() > deadline {
                        panic!(
                            "timed out: alpha={:?} beta={:?}",
                            clients_of(&entries, "alpha"),
                            clients_of(&entries, "beta")
                        );
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
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

    #[test]
    fn build_tree_nodes_marks_only_current_path() {
        use crate::session::{SessionTree, WindowTree};
        use plexy_glass_mux::{PaneId, WindowId};
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
        assert!(find(&|n| n.session == "cur" && n.window == Some(WindowId(1)) && n.pane.is_none()).is_current);
        assert!(!find(&|n| n.session == "cur" && n.window == Some(WindowId(0)) && n.pane.is_none()).is_current);
        assert!(find(&|n| n.pane == Some(PaneId(2))).is_current);
        assert!(!find(&|n| n.session == "cur" && n.pane == Some(PaneId(1))).is_current);
        // Label formats.
        assert_eq!(find(&|n| n.pane == Some(PaneId(2))).label, "pane 2: p");
        assert_eq!(find(&|n| n.session == "cur" && n.window == Some(WindowId(1)) && n.pane.is_none()).label, "2: w1");
        // The other (non-current) session's whole subtree is unmarked.
        assert!(nodes.iter().filter(|n| n.session == "other").all(|n| !n.is_current));
    }

    // `Ctrl+a W` then navigate to the *beta* window node and rename it, a
    // cross-session rename (the client is attached to alpha). Asserts the rename
    // landed on beta's `WindowManager` via a fresh `tree_snapshot` of beta.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn choose_tree_renames_window_in_other_session() {
        use std::time::{Duration, Instant};
        use tokio::io::{AsyncReadExt, split};

        let registry = Arc::new(crate::SessionRegistry::new());
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        let size = PtySize { rows: 16, cols: 60, pixel_width: 0, pixel_height: 0 };
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
        let server =
            tokio::spawn(async move { Connection::serve(server_side, 7, reg, cfg2).await });

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
            if entries.iter().find(|e| e.name == "alpha").map(|e| e.clients) == Some(1) {
                break;
            }
            if Instant::now() > deadline {
                panic!("alpha never registered a client");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
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
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        server.abort();
    }

    // Drive break-pane and join-pane through the full key/verb path and assert
    // the window/pane structure via `tree_snapshot` (the screen-scrape e2e harness
    // has no count API). split → break grows to 2 windows; mark + select + join
    // moves the pane back into window 0 and removes the emptied source window.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn break_and_join_panes_via_keys() {
        use std::time::{Duration, Instant};
        use tokio::io::{AsyncReadExt, split};

        let registry = Arc::new(crate::SessionRegistry::new());
        // Tests share the real persist dir; drop any saved "main" so this test
        // attaches a FRESH session rather than restoring accumulated state.
        let _ = crate::persist::delete_session("main");
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        let size = PtySize { rows: 16, cols: 60, pixel_width: 0, pixel_height: 0 };
        let cat = || SpawnSpec {
            program: "/bin/cat".into(),
            args: vec![],
            env: vec![],
            cwd: None,
        };

        let (server_side, client_side) = duplex(64 * 1024);
        let reg = Arc::clone(&registry);
        let cfg2 = Arc::clone(&cfg);
        let server =
            tokio::spawn(async move { Connection::serve(server_side, 7, reg, cfg2).await });

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
                        panic!("timed out: windows={w} panes={p} (want w={want_windows} p={want_panes})");
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
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
        assert_eq!(st.windows[0].active_pane, plexy_glass_mux::PaneId(1), "joined pane is active");

        server.abort();
    }

    // Push a paste buffer, then `Ctrl+a ]`: the bytes reach the active pane
    // (a `/bin/cat` pane echoes them, so they appear on its screen). And
    // `Ctrl+a =` + `d` deletes a buffer (the registry count drops).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn paste_buffer_reaches_pane_and_chooser_deletes() {
        use std::time::{Duration, Instant};
        use tokio::io::{AsyncReadExt, split};

        let registry = Arc::new(crate::SessionRegistry::new());
        let _ = crate::persist::delete_session("main"); // attach fresh, not restored
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        let size = PtySize { rows: 16, cols: 60, pixel_width: 0, pixel_height: 0 };
        let cat = || SpawnSpec {
            program: "/bin/cat".into(),
            args: vec![],
            env: vec![],
            cwd: None,
        };

        let (server_side, client_side) = duplex(64 * 1024);
        let reg = Arc::clone(&registry);
        let cfg2 = Arc::clone(&cfg);
        let server =
            tokio::spawn(async move { Connection::serve(server_side, 7, reg, cfg2).await });

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
            if Instant::now() > deadline {
                panic!("session never created");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // Inject a buffer (deterministic, so we avoid driving a live copy-mode yank).
        registry.push_paste_buffer(b"echoed-paste\n".to_vec()).await;
        // Ctrl+a ] pastes the newest buffer into the cat pane.
        Codec::write_frame(
            &mut cw,
            &postcard::to_allocvec(&ClientMsg::Input(bytes::Bytes::from_static(b"\x01]")))
                .unwrap(),
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
                    if Instant::now() > deadline {
                        panic!("pane never showed {needle:?}");
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
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
            if Instant::now() > deadline {
                panic!("chooser delete did not drop the buffer count");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        server.abort();
    }

    // The feature's primary entry point: a copy-mode yank pushes a paste buffer.
    // Drives the real key path (Ctrl+a [ to enter copy mode, `v` to start a
    // selection, `y` to yank) so the yank→push wiring is protected.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn copy_mode_yank_pushes_a_paste_buffer() {
        use std::time::{Duration, Instant};
        use tokio::io::{AsyncReadExt, split};

        let registry = Arc::new(crate::SessionRegistry::new());
        let _ = crate::persist::delete_session("main"); // attach fresh, not restored
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        let size = PtySize { rows: 16, cols: 60, pixel_width: 0, pixel_height: 0 };
        let cat = || SpawnSpec {
            program: "/bin/cat".into(),
            args: vec![],
            env: vec![],
            cwd: None,
        };

        let (server_side, client_side) = duplex(64 * 1024);
        let reg = Arc::clone(&registry);
        let cfg2 = Arc::clone(&cfg);
        let server =
            tokio::spawn(async move { Connection::serve(server_side, 7, reg, cfg2).await });

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
            if Instant::now() > deadline {
                panic!("session never created");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
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
            if Instant::now() > deadline {
                panic!("copy-mode yank did not push a paste buffer");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        server.abort();
    }

    // End-to-end through the production render coordinator (the sole caller of
    // update_monitor_flags): a BEL in a BACKGROUND window flags that window
    // (monitor-bell on by default), and switching to it clears the flag. Uses a
    // unique session name + delete-at-start for isolation from the shared persist
    // dir.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn background_bell_flags_window_via_coordinator() {
        use std::time::{Duration, Instant};
        use tokio::io::{AsyncReadExt, split};

        let registry = Arc::new(crate::SessionRegistry::new());
        let _ = crate::persist::delete_session("bellmon");
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        let size = PtySize { rows: 16, cols: 60, pixel_width: 0, pixel_height: 0 };
        let cat = || SpawnSpec {
            program: "/bin/cat".into(),
            args: vec![],
            env: vec![],
            cwd: None,
        };

        let (server_side, client_side) = duplex(64 * 1024);
        let reg = Arc::clone(&registry);
        let cfg2 = Arc::clone(&cfg);
        let server =
            tokio::spawn(async move { Connection::serve(server_side, 7, reg, cfg2).await });

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

        // Ctrl+a c → window 1 (active); Ctrl+a p → back to window 0 (window 1 now
        // background). Poll until established.
        Codec::write_frame(
            &mut cw,
            &postcard::to_allocvec(&ClientMsg::Input(bytes::Bytes::from_static(b"\x01c\x01p")))
                .unwrap(),
        )
        .await
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(s) = registry.get("bellmon").await {
                let m = s.window_manager.lock().await;
                if m.windows().len() == 2 && m.active_idx() == 0 {
                    break;
                }
            }
            if Instant::now() > deadline {
                panic!("two windows with window 0 active never established");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
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
            if Instant::now() > deadline {
                panic!("the coordinator never flagged the background window's bell");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // Switching to window 1 (Ctrl+a n) clears its flag.
        Codec::write_frame(
            &mut cw,
            &postcard::to_allocvec(&ClientMsg::Input(bytes::Bytes::from_static(b"\x01n")))
                .unwrap(),
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
            if Instant::now() > deadline {
                panic!("bell flag did not clear after switching to the window");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        server.abort();
    }
}
