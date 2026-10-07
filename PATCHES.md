# MXM patch set for baseview 0.3.6

Upstream crate: `baseview 0.3.6`, MIT OR Apache-2.0, as published on crates.io (the first commit
here). The upstream README and both licence texts remain unchanged. Only egui-baseview depends on
baseview in the collection, so the egui-baseview fork takes this fork by git and every plugin gets
it through that one dependency.

## Frames a busy host cannot starve (Windows)

**File:** `src/platform/win/window.rs`; every hunk contains `MXM PATCH`.

On Windows baseview drew each frame on `WM_TIMER`, the lowest-priority message there is: Windows
only synthesises it when the thread's queue holds nothing else, a pending `WM_PAINT` included. A
plugin's window lives on the host's GUI thread, so a host that always has its next frame queued
starves the plugin's window of frames, and egui reads the mouse and keyboard only in a frame. The
collection hit it three times: MXM Player's editors drew white (the kit's `docs/known-issues.md`,
*A plugin editor renders white when the host repaints too often*, worked around in the player by
asking for frames 50 ms apart), and on 2026-10-07 an MXM editor in newDAWn ignored the mouse while
the song played, newDAWn redrawing back to back for its playhead (worked around there too, with
frames 33 ms apart). Hosts the collection does not control may do the same.

The patch:

- adds a frame clock, one thread per window, that posts `BV_FRAME` (`WM_USER + 2`) every 15 ms.
  A posted message is retrieved before input, `WM_PAINT` and `WM_TIMER`, so it arrives whatever the
  host does; at most one is waiting at a time, so a stalled window builds no backlog;
- draws a frame on `BV_FRAME`, and keeps `WM_TIMER`'s frames as the fallback, paced so the two
  never draw twice within 10 ms;
- stops and joins the clock when the window is destroyed, before its handle can be reused.

The timer itself stays where upstream sets it (its placement orders a parent and child window's
drawing, upstream's own `FIXME` says). macOS and X11 are untouched.

## Refresh/drop procedure

1. Extract a new published baseview over this folder, retaining this file.
2. Check whether its Windows window still draws frames only on `WM_TIMER`.
3. If upstream now draws frames in a way a busy host cannot starve, point the egui-baseview fork
   back at crates.io and archive this repository.
4. Otherwise reapply only the `MXM PATCH` hunks, build an editor, and check it in a host that
   redraws back to back: MXM Player with `MXM_SERVICE_MS=16` must not draw white.
