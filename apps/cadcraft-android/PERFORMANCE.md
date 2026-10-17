# Tablet rendering and history

Navigation uses temporary screen geometry: patterned hatches show their boundaries, gradients
use a flat preview, text shows its extent, and curves use a coarser chord tolerance. After
180 ms without view movement, a detailed rebuild is requested on the existing bounded worker.
Screen geometry also replaces subpixel thin curves with point markers. The drawing database,
engine snapping and file/export geometry are unchanged.

Tablet Settings offers independent switches for simpler navigation previews and reduced
navigation resolution, plus a 15–60 fps drawing refresh setting (30 by default). Reduced
resolution uses up to 65% of the native width and height while navigating; interface controls
and live overlays retain native resolution. The previous image is reprojected between drawing
refreshes, so the refresh limit never sleeps or delays pen input. Idle rendering stays driven
by events and existing host timers.

The main drawing can be retained as one transparent GPU image below cursor, snap, selection
and command overlays. Changes to content, view, background, clipping, size or DPI invalidate
it. The texture shares the retained GPU allowance with inactive mesh buffers. If a sharp
stationary image cannot fit, the canvas draws directly at native resolution. Active clipped
viewport contents retain their existing stencil rendering path.

Finite repeated blocks share GPU geometry and supply an affine placement per copy. Resolved
styles are part of template identity; continuous geometry can share across translations,
rotations, reflections and unequal scales. Patterned linetypes retain baked linear transforms
to preserve dash lengths. Masks, annotative or attribute-bearing references, oversized
templates and isolated occurrences retain the regular mesh path. Only consecutive compatible
placements are grouped, and wipeout barriers retain their drawing order. CPU display geometry remains available for
highlights and the software fallback.

Conservative mesh bounds and per-block bounds reject invisible geometry before submitting
draws. Adjacent visible ranges merge again to avoid extra draw calls when a buffer is entirely
visible. Opaque batches use replacement writes; transparent batches retain premultiplied
blending and their existing order.

Undo and redo retain at most **20 logical undo steps** and have a memory allowance. UNDO
groups keep their starting state as one step, including scripts longer than 20 commands.
Allocation identities deduplicate COW chunks, entities, indexes, block definitions and foreign
data. Live and saved drawings are excluded from the additional history charge. The engine
defaults to 32 MiB; Android shares 8–64 MiB across open drawings, derived from twice the
effective render-cache allowance, with a larger share for the active drawing. Old history is
discarded when necessary; current drawing data is preserved. Variable payload sizes include
conservative estimates and are not allocator telemetry.

Validation uses the existing repository checks plus private rendering comparisons on software
Vulkan and OpenGL ES: repeated blocks, wide lines, transparency, cross-chunk wipeouts,
stationary cache reuse, navigation pacing/detail restoration, pressure fallback, culling and
history sharing. These checks establish behavior; device-specific GPU time, battery use and
thermal improvements still require measurements on Android hardware.
