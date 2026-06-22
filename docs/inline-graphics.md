# Inline graphics

plexy-glass renders **images inside panes** instead of mangling them the way
most multiplexers do. When a program emits a terminal graphics protocol,
plexy-glass captures it, models the image as a placement anchored in the grid,
and re-emits it to each attached client's terminal at the right cell, sized
correctly, with the next prompt below it, and following as you scroll.

This is built in phases. **The Kitty graphics protocol is supported.** Images
are clipped to their visible region, follow copy/block-mode scrolling, fall
back to a labelled placeholder box on terminals without graphics, and support
Kitty Unicode-placeholder (virtual) placements. Sixel and iTerm2, and native
Kitty animation, arrive in later phases (see
`docs/superpowers/specs/2026-06-22-inline-graphics-design.md`).

## Trying it

In a Kitty-graphics-capable outer terminal (e.g. Ghostty), inside a pane:

```
timg -p kitty <image>      # or: kitten icat <image>, chafa -f kitty <image>
```

The image renders in the pane, scales to plexy-glass's cells, and the shell
prompt lands below it. Scrolling moves the image with the content; scrolling it
off-screen removes it.

## How it works

- **Cell size relay.** The client reports its terminal's pixel size; the daemon
  sizes each pane's PTY with it and answers `CSI 14t/16t/18t` so a program like
  `timg` scales to plexy-glass's *real* cell size. (Phase 1.)
- **Capture.** The emulator pulls Kitty graphics APC sequences (`ESC _ G … ESC \`)
  out of the byte stream, accumulates chunked transmissions into an image store,
  and records a placement at the cursor, advancing the cursor by the image's
  cell footprint so following output lands below it.
- **Per-client render.** Each attached client gets the image transmitted **once**
  and then **placed by id**; a per-frame diff re-places it as it scrolls and
  deletes it when it leaves the viewport. The image's cell box is forced (`r/c`)
  so it occupies the same cells on every client regardless of that client's own
  cell pixel size.
- **Clipping.** A placement is clipped to its visible sub-rectangle (the
  viewport rows intersected with the pane's columns) and the source pixels are
  cropped to match (Kitty `x/y/w/h`), so an image taller or wider than the space
  left in the pane never overruns the next pane, a split border, or the status
  bar, and an image scrolled partway off the top shows its visible lower part.
- **Cross-mode.** Images follow copy-mode and block-mode scrolling (those are
  per-pane viewports, not modal overlays). While an interactive overlay (command
  prompt, picker, choose-tree/buffer, rename, help) or a popup is open, images
  are suppressed (the modal owns the screen) and re-established when it closes.
- **Capability negotiation + placeholder box.** At attach, the client probes its
  terminal for graphics support and relays it. A client whose terminal lacks
  Kitty graphics is shown a **placeholder box**, a labelled rectangle of the
  image's exact footprint, instead of blank cells, so a session attached by both
  a graphics terminal and a plain one keeps a consistent layout.
- **Unicode-placeholder (virtual) placements.** Apps that use Kitty's Unicode
  placeholder mode (`a=p,U=1` + `U+10EEEE` cells) are supported: the image is
  transmitted once and the virtual placement emitted once per client, and the
  placeholder cells scroll/reflow with the text natively.
- **Animation.** Tools that animate by re-transmitting each frame under the same
  image id (e.g. `timg`, `chafa` on a GIF) play back correctly, since a changed
  image is re-transmitted to each client automatically. The *native* Kitty
  animation protocol (`a=f` frames / `a=a` control) is not yet implemented.

## Limitations

- Kitty graphics only. Sixel and iTerm2 inline images are Phase 5.
- Images are suppressed on the alternate screen.
- Native Kitty animation (`a=f`/`a=a`), explicit `z`-ordering, relative
  placements, and a popup rendering its *own* inline images are future work.
- Two panes that both use Unicode-placeholder mode with the same raw image id
  can collide on one client (the placeholder cells carry the raw id).
- A pane resize drops its **classic** placements (the program re-emits on
  redraw); a reflow-aware anchor remap is later lifecycle work.
  Unicode-placeholder (virtual) placements survive resize, since the
  placeholder cells reflow with the text.
- Transmitted image *pixel data* is not persisted across a daemon restart.
