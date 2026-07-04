# Inline graphics

plexy-glass renders **images inside panes** instead of mangling them the way
most multiplexers do. When a program emits a terminal graphics protocol,
plexy-glass captures it, models the image as a placement anchored in the grid,
and re-emits it to each attached client's terminal at the right cell, sized
correctly, with the next prompt below it, and following as you scroll.

All three common terminal graphics protocols are supported: **Kitty graphics**,
**Sixel**, and **iTerm2** (`OSC 1337`). Images are clipped to their visible
region, follow copy/block-mode scrolling, and fall back to a labelled
placeholder box on a client whose terminal can't render that image's protocol.
Kitty additionally supports Unicode-placeholder (virtual) placements, native
animation playback, and `z=` stacking order (the latter also honored for
Sixel/iTerm2, see below).

## Trying it

In a Kitty-graphics-capable outer terminal (e.g. Ghostty), inside a pane:

```
timg -p kitty <image>      # or: kitten icat <image>, chafa -f kitty <image>
```

Sixel and iTerm2 work too, on terminals that support them:

```
img2sixel <image>          # Sixel (DCS)
chafa -f sixel <image>
imgcat <image>             # iTerm2 (OSC 1337), in iTerm2/WezTerm
```

The image renders in the pane, scales to plexy-glass's cells, and the shell
prompt lands below it. Scrolling moves the image with the content; scrolling it
off-screen removes it. An image is only sent to a client whose terminal speaks
that image's protocol; other clients see a labelled placeholder box of the same
size.

## How it works

- **Cell size relay.** The client reports its terminal's pixel size; the daemon
  sizes each pane's PTY with it and answers `CSI 14t/16t/18t`, so a program
  like `timg` scales to plexy-glass's *real* cell size. (Phase 1.)
- **Capture.** The emulator pulls each protocol out of the byte stream into one
  unified image model: Kitty graphics from APC (`ESC _ G … ESC \`), Sixel from
  DCS (`ESC P … q … ESC \`), and iTerm2 from `OSC 1337 ; File=`. Each image
  records its **source protocol** and pixel dimensions (Kitty `s/v` or the
  PNG/JPEG/Sixel-raster header), and a placement is recorded at the cursor,
  advancing the cursor by the cell footprint so following output lands below
  it.
- **Per-client render, by protocol.** Rendering is per client, gated on the
  client's support for each image's source protocol, and there is **no
  transcoding**. A Kitty image is transmitted **once** and **placed by id**
  with a per-frame diff (re-place on scroll, delete when off-screen, forced
  `r/c` cell box so it occupies the same cells on every client). Sixel and
  iTerm2 have no place-by-reference model, so their data is re-emitted at the
  host cell and the old region repainted when they move. Note that this is
  best-effort, those protocols are heavier; Kitty is the precise fast path.
- **Clipping.** A placement is clipped to its visible sub-rectangle (the
  viewport rows intersected with the pane's columns) and the source pixels are
  cropped to match (Kitty `x/y/w/h`), so an image taller or wider than the
  space left in the pane never overruns the next pane, a split border, or the
  status bar, and an image scrolled partway off the top shows its visible
  lower part.
- **Cross-mode.** Images follow copy-mode and block-mode scrolling (those are
  per-pane viewports, not modal overlays). While an interactive overlay
  (command prompt, picker, choose-tree/buffer, rename, help) or a popup is
  open, images are suppressed (the modal owns the screen) and re-established
  when it closes.
- **Capability negotiation + placeholder box.** At attach, the client probes
  its terminal and relays which protocols it supports: Kitty (graphics query),
  Sixel (DA1 attribute), iTerm2 (`TERM_PROGRAM` fingerprint, iTerm2/WezTerm).
  A placement whose protocol the client can't render is shown a **placeholder
  box**, a labelled rectangle of the image's exact footprint, instead of blank
  cells, so a session attached by both a graphics terminal and a plain one
  keeps a consistent layout.
- **Unicode-placeholder (virtual) placements.** Apps that use Kitty's Unicode
  placeholder mode (`a=p,U=1` + `U+10EEEE` cells) are supported: the image is
  transmitted once and the virtual placement emitted once per client, and the
  placeholder cells scroll/reflow with the text natively.
- **Animation, native (Kitty).** The emulator captures every `a=f` frame
  command and the latest `a=a` control state verbatim (frame data, canvas
  source, gap timing, and the stop/loading/loop state plus loop count and
  current-frame jump), without compositing any pixels itself. The per-client
  renderer replays the frame log and control state to each Kitty client (the
  whole log to a newly-attached client, only the new frames plus a changed
  control state to one already attached) and then **stops re-transmitting** —
  from there the client's own terminal plays the animation (looping,
  stopping, jumping to a frame) exactly as it would for a directly-connected
  program.
- **Animation, re-transmit workaround (Sixel/iTerm2).** These protocols have
  no native animation model, so tools that animate by re-transmitting each
  frame under the same image id (e.g. `timg`, `chafa` on a GIF) keep working
  the same way they always have: a changed image is re-transmitted to each
  client automatically. This path is unchanged and is Sixel/iTerm2-only now
  that Kitty has real native playback.
- **`z=` ordering.** Kitty clients get a placement's `z` passed straight
  through on the wire (omitted when `0`, the default, since that's already
  Kitty's default stacking order); ties and negative-vs-text stacking are
  left to the client's own terminal. Sixel and iTerm2 have no placement
  protocol of their own to carry a stacking order, so before drawing,
  overlapping placements on those clients are sorted by `(z, image_id)` so
  lower `z` (and, within the same `z`, lower image id) draws first and higher
  draws on top.

## Limitations

- Images are suppressed on the alternate screen.
- No cross-protocol transcoding: a Sixel image is shown only to Sixel-capable
  clients (a placeholder box otherwise), and likewise for Kitty/iTerm2.
- Sixel and iTerm2 images aren't source-cropped under partial occlusion (they're
  emitted at their visible top-left and the terminal clips at the screen edge);
  precise crop is Kitty-only.
- Sixel and iTerm2 have no native animation of their own; they rely on the
  re-transmit-under-the-same-id workaround, same as before.
- Relative placements and a popup rendering its *own* inline images are still
  future work.
- Two panes that both use Unicode-placeholder mode with the same raw image id
  can collide on one client (the placeholder cells carry the raw id).
- A pane resize drops its **classic** placements (the program re-emits on
  redraw); a reflow-aware anchor remap is later lifecycle work.
  Unicode-placeholder (virtual) placements survive resize, since the
  placeholder cells reflow with the text.
- Transmitted image *pixel data* is not persisted across a daemon restart.
