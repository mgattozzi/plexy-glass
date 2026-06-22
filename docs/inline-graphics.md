# Inline graphics

plexy-glass renders **images inside panes** instead of mangling them the way
most multiplexers do. When a program emits a terminal graphics protocol,
plexy-glass captures it, models the image as a placement anchored in the grid,
and re-emits it to each attached client's terminal at the right cell, sized
correctly, with the next prompt below it, and following as you scroll.

This is built in phases. **The Kitty graphics protocol is supported, with
images clipped to their visible region and following copy/block-mode
scrolling.** Sixel and iTerm2, animation, and Unicode-placeholder placements
arrive in later phases (see
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
- **Capability negotiation.** At attach, the client probes its terminal for
  graphics support and relays it. A client whose terminal lacks Kitty graphics
  is sent no image bytes (it sees blank cells where the image would be); a
  richer placeholder is later-phase work.

## Limitations

- Kitty graphics only. Sixel and iTerm2 inline images are Phase 5.
- Images are suppressed on the alternate screen.
- Animation and Unicode-placeholder (virtual) placements, explicit `z`-ordering,
  and a popup rendering its *own* inline images are Phase 4+.
- A pane resize drops its images (the program re-emits on redraw); a
  reflow-aware anchor remap is later lifecycle work.
- Transmitted image *pixel data* is not persisted across a daemon restart.
