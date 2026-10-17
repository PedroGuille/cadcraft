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
to preserve dash lengths. Attribute-bearing references and MINSERT arrays share their fixed body; unique attributes
remain in their original order and use their own resolved styles. Masks and annotative references
retain the full builder. Oversized or isolated compatible templates can still share mandatory
active geometry without charging cold retained caches. Only consecutive compatible
placements are grouped, and wipeout barriers retain their drawing order. CPU picking, highlights and the software fallback visit shared templates with affine placements;
only the points actually requested by a consumer are materialized.

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

## Shared geometry and tiled updates (Android 0.1.1)

Screen scenes store block and filled outline-font templates once, with per-occurrence transforms
and top-level handles. A logical primitive iterator and random access resolve world coordinates
only for software painting or highlights. Flattening remains an explicit compatibility operation;
engine snapping and all exports keep their full geometry. Weak template indexes let active
placements continue sharing even when optional retention is zero. GPU placements keep their
shared template buffers alive independently of the cold-cache allowance. Consecutive equal
glyphs are instanced; isolated glyphs join the ordinary upload to avoid a draw per character.

The stationary drawing image is a tiled atlas with 512-pixel damage regions. Changed chunks
invalidate both their old and new bounds; deletion, movement and undo erase old pixels. Dirty
tiles are cleared to transparency and replay every applicable draw-order group, including
wipeouts, with pre-submit visibility culling. Reordered chunks, changed display styles, scale,
DPI or clipping conservatively request a complete refresh. Bounds are conservative at chunk
rather than individual-entity granularity, so a dispersed chunk can still dirty many tiles.

Integral-pixel pans scroll retained image pixels through a single scratch tile (at most 1 MiB)
and replay exposed tiles. Fractional motion and zoom use the existing reprojection/refresh
path; the original detailed image returns at rest. Texture and scratch storage share the GPU
allowance. If they cannot fit, scrolling is skipped or direct native-resolution drawing is used.

The tablet host now includes both reusable glyph outlines and tessellated meshes in its shared
cache allowance. At the default 24 MiB, the split is GPU/image 16 MiB, glyph geometry 3 MiB,
spatial indexes 2 MiB, snapping 1 MiB, and remaining CPU caches 2 MiB. This preserves the
previous GPU allowance so sharp canvas images still fit on higher-resolution tablets. Glyph geometry gives one
third to outlines and two thirds to tessellation. Memory-pressure trimming discards cold entries;
strong references from active geometry remain valid. These allowances describe reusable payload,
not a hard process-memory cap or allocator telemetry. Desktop defaults remain unchanged.
