# Detachable multi-monitor workspaces

Maelstrom's detachable panels are native operating-system windows backed by the same editor
session. They are not extra editor instances and they do not own independent project, playback,
decode, undo, autosave, or export state.

## Available detachable sections

The Edit workspace exposes eight independently detachable sections:

- Media Pool
- Viewer
- Timeline
- Inspector
- Audio
- Color
- Effects
- Media details

Undertow exposes two additional native sections while reusing the same detachable Timeline:

- Undertow Tools and track selector
- Undertow Mixer

The shared Timeline keeps one project, playback, decoder, and undo authority. Its dock destination
is saved separately for Edit and Undertow: Bottom remains the Edit default, while Center remains
the Undertow default. Active dock tabs are also scoped per workspace, so arranging audio panels
does not rearrange or change the selected tabs in Edit.

The existing organized single-window layout remains the default, including its compact five-tab
right sidebar. Detaching any sidebar tab promotes only that tab into its own native window; the
other tabs remain available and independently detachable. A detached section disappears
from that layout and the remaining sections expand into the released space. Its native window can
be moved or resized on any connected monitor. The View > Panels menu and detached-panel Dock menu
can return it to the Left, Center, Right, or Bottom region. Panels sharing a region become tabs;
docking selects the panel that just arrived. The native close button returns a panel to its saved
dock region.

## Ownership rules

- `nle-ui-core::EditorState` remains the single mutable editor authority.
- All windows share one egui context and texture namespace so focus and drag payloads remain
  coherent between panels.
- All swap-chain surfaces share one wgpu device, queue, and `HubRenderer`; timeline and Viewer
  callback resources are never duplicated per panel.
- Playback advances once per application frame. A detached Viewer only presents the already
  selected monitor layers; it does not request a second decode.
- Root-window close exits after the normal autosave flush. Closing a detached window reattaches
  only that panel.

## Native host contract

Each child viewport has its own native window, surface configuration, and `egui_winit::State`.
Window events must be routed by `WindowId`; resize, DPI, focus, keyboard, pointer, file-drop, and
close events must never fall through to the root window by accident. Child paints are queued until
the retained timeline and Viewer canvas data has been submitted, then all surfaces are painted
serially through the shared renderer.

egui texture additions are applied before any viewport paint and removals happen only after every
viewport has painted. This preserves the one-context texture lifetime contract.

## Completion gates

The slice is complete only when:

1. each Edit section plus Undertow Tools and Mixer can detach into a decorated native window and
   reattach;
2. the remaining root layout expands without clipped borders or dead gaps;
3. child close does not exit the application and root close still does;
4. edits made in any panel use the same selection, undo, autosave, and project snapshot;
5. Viewer and Timeline surfaces keep their retained native rendering path;
6. transition drag can begin in detached Effects, end in detached Timeline, apply once, and undo;
7. focused unit and workspace tests pass through the project runtime runner; and
8. no Maelstrom, Cargo, compiler, FFmpeg, or test process is left running.

## Workspace persistence

Dock destinations and detached membership are backward-compatible project view state. Projects
saved by the earlier combined-Tools build migrate that dock destination to all five sidebar
sections. If the combined Tools window was detached, its runtime-only tab selection would have
restored to Inspector, so the migration detaches Inspector and leaves the other four sections
docked. The old machine-local Tools geometry follows Inspector and never overwrites a newer
Inspector placement. Edit and
Undertow dock maps are stored independently; older projects retain their Edit layout and receive
the organized Undertow defaults. Native
window geometry is saved separately in the machine-local Maelstrom preferences as desktop-space
physical position, logical size, DPI scale, and monitor identity. Moving or resizing a native panel
therefore does not dirty or autosave the project. Restore only geometry that intersects an available
monitor; if the old monitor moved or disappeared, center the panel on the same named monitor when
available, otherwise on the primary display, and persist that corrected placement.
