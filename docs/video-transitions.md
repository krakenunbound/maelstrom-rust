# Video transition families

Maelstrom stores video transitions as bounded operations at exact adjacent cuts. The Effects panel
and the timeline context menu expose the same nested English/Japanese catalog. A transition can be
dragged from Effects onto a valid unused video cut or applied from the context menu; either route is
one undoable edit and uses the same source-handle validation.

The current native catalog has 16 durable kinds:

- Dissolve: Cross Dissolve and Film Dissolve.
- Fade: Dip to Black and Dip to White.
- Wipe: Left, Right, Up, and Down.
- Slide: From Left, Right, Top, and Bottom.
- Push: From Left, Right, Top, and Bottom.

Slide and Push are deliberately distinct. Slide moves the incoming clip over a stationary outgoing
clip. Push moves both continuously. At shaped transition progress `p`, Push From Left positions the
incoming and outgoing clips at `p - 1` and `p` frame widths; the other directions apply the mirrored
equivalent on the horizontal or vertical axis. Preview and export use those same directional
expressions while both sources continue advancing.

Existing serialized transition names remain unchanged. The four Push variants extend the current
document format without a schema bump; save/reopen coverage exercises all 16 kinds with their exact
adjacent clip IDs, duration, and curve.

## Verification boundary

Deterministic preview tests cover all Push directions at the start, midpoint, and half-open end.
Export graph tests cover all 16 kinds, including a middle clip with an incoming Slide and outgoing
Push. A real bundled-FFmpeg render samples both moving sources at the midpoint of all four Push
directions. This proves the new Push behavior and its headless preview/export contract.

It does not close the broader Phase 4 exit gate. Full encoded-pixel parity for every direction,
stacked transformed tracks, titles, the Rec.709 pipeline, native viewer-window presentation, and
transition-heavy sustained performance remain separate qualification work.
