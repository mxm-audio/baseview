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
- draws a frame on `BV_FRAME`, and keeps `WM_TIMER`'s frames as the fallback;
- **leaves the thread a gap after every frame**: on the window's own thread, for both messages, no
  frame starts within 10 ms of the last one's start or 5 ms of its end, and the clock holds its one
  message's claim until that frame has ended. A posted message is retrieved before input, so
  without the gap any frame longer than the clock's 15 ms — every vsynced present on a screen under
  about 66 Hz — found the next one already posted, and the window drew back to back while the
  host, its other windows and the mouse waited (MXM Player froze on a 4K, 59 Hz screen,
  2026-10-14). The clock's reading of the frame times only decides when it wakes: one it posts
  early is refused, and costs a wake-up;
- stops and joins the clock at `WM_DESTROY`, before Windows frees the handle, and not only when the
  window drops, which a `DestroyWindow` called from inside one of the window's own messages puts
  after it.

The timer itself stays where upstream sets it (its placement orders a parent and child window's
drawing, upstream's own `FIXME` says). macOS and X11 are untouched by this patch.

## Transient for, and the keys given back (every platform)

**Files:** `src/window.rs`, `src/context.rs`, `src/wrappers/win32/window/handle.rs`,
`src/platform/win/{window,window_state}.rs`, `src/platform/macos/{context,view}.rs`,
`src/platform/x11/{event_loop,window_shared,xcb_connection,xcb_window}.rs`; every hunk contains
`MXM PATCH (*Transient for, and the keys given back*)`.

An MXM editor's keyboard cursor leaves its window when VIEW goes past its edge, and the keys go
back to the host's window (newDAWn's keyboard-reach plan, S11, 2026-10-15). The window that owns
the editor's is the one the system knows: the owner of the top-level window an embedded editor is
in, or the window a floating one is transient for. The patch:

- adds `WindowContext::focus_owner()`, a request carried out after the handler's call, as
  `request_close` is (Windows: a posted `BV_FOCUS_OWNER`; macOS: the main queue). Done at once,
  inside a frame, it activated the owner on the same thread there and then; the editor heard it lost
  the focus while egui-baseview's state was still borrowed, and the panic, in a window procedure,
  aborted the host (newDAWn, tried 2026-10-15). Windows brings the owner (`GW_OWNER` of the root;
  `GA_ROOTOWNER` misses an overlapped owned window) to the foreground, or focuses the parent of an
  embedded window with no owner; macOS makes the parent window key, or the superview first
  responder; X11 asks for `_NET_ACTIVE_WINDOW` on `WM_TRANSIENT_FOR`, or focuses the parent;
- makes `Window::set_parent` on a window opened floating set it **transient for** that window
  instead (Windows: its owner; macOS: a child window of the parent's; X11: `WM_TRANSIENT_FOR`),
  where upstream panicked (Windows) or closed the window and embedded its view (macOS). CLAP's
  `gui.set_transient` reaches a floating editor's window only through nice-plug's
  `EditorHandle::set_parent` (the nice-plug fork's *Floating windows*), so this is its channel;
- on X11, asks for a floating window to be active when it is shown.

Windows is tried in newDAWn (an embedded MXM editor: VIEW out, OPEN back, VIEW out again);
macOS and X11 type-check only, for the owner's batch.

## Keys the editor ignores go to the host (Windows)

**Files:** `src/platform/win/{window,window_state}.rs`; every hunk contains
`MXM PATCH (*Keys the editor ignores go to the host*)`.

Upstream's Windows window answers every key message it is sent as handled, whatever the window's
handler says of the key, so a key an embedded editor doesn't use goes nowhere. baseview's macOS view
already gives an ignored key to the next responder; JUCE's windows post an unhandled one to their
parent (`forwardMessageToParent`). Ableton Live 11.3.25 hands a plugin window every key while it has
the focus (measured 2026-10-16) and acts on those that come back: with ValhallaDelay's JUCE window
focused, T played and Space started the transport; with an MXM editor's, neither did. The patch:

- posts `WM_KEYDOWN`, `WM_CHAR` and `WM_KEYUP` the handler answers `EventStatus::Ignored` to the
  window's parent (`GetAncestor(GA_PARENT)`), for a window embedded in another only; a top-level
  window, floating, keeps upstream's behaviour;
- holds back a key-down whose character follows, and gives it with that character or not at all,
  since the character is where the handler's answer comes;
- sends a key-up the way its key-down went, whatever the handler says of it, so the host never sees
  half a key;
- leaves `WM_SYS*` keys to Windows' default handling, which already takes them to the top level.

What a window answers is its handler's business: egui-baseview's fork asks the app
(*Keys the app uses, asked*). Tried in Ableton Live 11.3.26 (VST3) and Bitwig 5 (CLAP), 2026-10-16:
T and Space go to the host, the editor's keys and arrows stay. `KeysGivenBack`'s tests hold the
decision.

## Refresh/drop procedure

1. Extract a new published baseview over this folder, retaining this file.
2. Check whether its Windows window still draws frames only on `WM_TIMER`.
3. If upstream now draws frames in a way a busy host cannot starve, point the egui-baseview fork
   back at crates.io and archive this repository.
4. Otherwise reapply only the `MXM PATCH` hunks, build an editor, and check it in a host that
   redraws back to back: MXM Player with `MXM_SERVICE_MS=16` must not draw white, and on a 60 Hz
   screen its windows must still move and take clicks with the editor open.
5. Whatever upstream has done about frames, reapply *Transient for, and the keys given back*
   unless upstream has its own way to activate a window's owner and to make a floating window
   transient, and try VIEW out of an MXM editor in newDAWn and back with OPEN.
6. Reapply *Keys the editor ignores go to the host* unless upstream's Windows window gives an
   ignored key to its parent, and try T and Space in an MXM editor in Ableton Live.
