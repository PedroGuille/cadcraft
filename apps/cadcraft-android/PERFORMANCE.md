# Tablet rendering and history

Navigation retains the same text, hatch, gradient and curve geometry detail as the stationary
view, with no navigation-specific substitutions or coarser chord tolerance. Screen geometry
still replaces subpixel thin curves with point markers. The drawing database, engine snapping
and file/export geometry are unchanged.

Tablet Settings offers reduced navigation resolution and a 15–60 fps drawing refresh setting
(30 by default). Reduced resolution uses up to 65% of the native width and height while
navigating; interface controls and live overlays retain native resolution. The previous image
is reprojected between drawing refreshes, so the refresh limit never sleeps or delays pen input.
After 180 ms without view movement, navigation ends and the native-resolution image returns.
Idle rendering stays driven by events and existing host timers.

The main drawing can be retained as transparent tiled GPU images below cursor, snap, selection
and command overlays. Content identities, background, clipping, size and DPI determine whether
pixels can be reused; incompatible views refresh. The images share the retained GPU allowance
with inactive mesh buffers. If a sharp
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
stationary cache reuse, navigation pacing/resolution restoration, pressure fallback, culling and
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
cache allowance. Android 0.1.1 used a default 24 MiB split: GPU/image 16 MiB, glyph geometry 3 MiB,
spatial indexes 2 MiB, snapping 1 MiB, and remaining CPU caches 2 MiB. Android 0.1.3 replaces
that small default with the adaptive allowance below. Glyph geometry gives one
third to outlines and two thirds to tessellation. Memory-pressure trimming discards cold entries;
strong references from active geometry remain valid. These allowances describe reusable payload,
not a hard process-memory cap or allocator telemetry. Desktop defaults remain unchanged.

## Entity damage, spatial ranges and vector text (Android 0.1.2)

Changed chunks carry rendered bounds and immutable source identities for their individual
entities. The image cache compares only replaced chunks, retaining unchanged chunks as cheap
draw-order anchors. Editing, moving, deleting and undoing an object invalidates its old and new
bounds rather than the entire scattered chunk. A changed rendering context invalidates affected
entities even when their source objects are unchanged. Reordering, unknown/non-finite bounds and
oversized comparisons conservatively refresh the image. Source references keep identity safe
through snapshot replacement and undo.

Each ordinary 384-record GPU range has a compact spatial hierarchy over 24-record ranges.
Visibility queries return surviving ranges in their original order; fully visible ranges still
merge into a single draw. Lines and triangles are never split across primitive boundaries, and
wide-line margins, transparency, stencil lanes and wipeout barriers retain their semantics.
Index payload is included in uploaded-mesh retention accounting.

The built-in drafting font now records canonical glyph placements alongside its compatibility
polylines. Interactive scenes share these stroke templates through the existing glyph budget,
including transformed TEXT/MTEXT. Decorations, missing-glyph boxes, subpixel stroke simplification
and precision-sensitive placements retain the full path. Exports and engine geometry are unchanged.

Mixed characters use shared vector geometry pages on the GPU, rather than copying each glyph or
issuing one draw per character. A page holds at most 16,384 float records (256 KiB); a placement
holds its affine transform, glyph range, colour and width. Filled outlines, hairlines and wide
strokes have separate lanes. Consecutive compatible placements batch without sorting CAD content.
Per-glyph bounds reject invisible placements; groups cap padding work and placement counts.
Pages are created only when shared geometry amortizes their texture and placement payload.
Rare complex outlines, oversized/unsupported glyphs and precision-sensitive transforms use the existing upload path.
The shader uses a vertex-readable float texture supported by GLES 3 and WebGL2, without storage
buffers, raster glyphs or loss of vector detail. Pages share the existing block-cache allowance;
weak lookups preserve active sharing when optional retention is zero. Worker uploads use the
render queue, so a newly visible page can be prepared off the UI thread.

## Retained views and adaptive 2 GiB allowance (Android 0.1.3)

The requested reconstructible-cache ceiling is now **2,048 MiB**, allocated only when useful
content is encountered. Existing tablet preferences migrate once; subsequent user changes are
preserved. The slider supports 8–2,048 MiB. This is not a 2 GiB reservation or a process-memory
cap: live drawings, active GPU geometry, drivers and undo have their own costs.

At full allowance, the split is GPU/images 1,024 MiB, glyph geometry 64 MiB, spatial indexes
128 MiB, snapping 32 MiB and CPU geometry/meshes/panel/grid models 800 MiB. Up to 8 MiB for grid
meshes and 4 MiB for palette models are reserved from that CPU share. GPU block geometry and
vector text pages can retain more templates; CPU block templates can retain up to 128 MiB
within their existing parent allowance. Shared instances still reference the same geometry.

Android checks available memory and process RSS every five seconds. Initial growth leaves a
quarter of physical RAM for system headroom and aims to keep the process below half of RAM
(up to 4 GiB). These are soft headroom checks, not an Android allocation guarantee. Low-memory
signals drop optional retention to zero; pressure reduces the allowance, and recovery increases
it in 128 MiB increments. Hysteresis avoids repeatedly evicting a useful cache merely because
its own allocations reduced free memory. Missing telemetry cannot trigger unchecked growth.
Exact live data is never trimmed. Undo stays at 20 steps with its separate session allowance.

Up to 32 compatible past tiled images can survive distant pans, zoom changes and native/reduced
resolution transitions, bounded together by the image share (normally half the GPU allowance).
Textures are moved into history, without an extra full-image copy. Cumulative pans checkpoint
visited areas after three quarters of a canvas, so gradual navigation also retains history. Returning to an identical
view and immutable content reuses its pixels; overlapping views can scroll retained tiles.
Styles, DPI, clipping, transforms and complete ordered content identities gate reuse. Missing
metadata or changed contents refresh conservatively. Old image metadata drops entity references.

Ordinary geometry chunks touching a visited area can build without clipping to that view, then
rely on the existing pre-submit GPU culling. Entirely distant chunks keep conservative culling;
pressure disables the additional prefetch. Panning can reuse complete visited chunks. Clipped hatch/block chunks and up to 32
view/zoom variants can survive revisits under the CPU allowance; incompatible settings and
changed chunks rebuild at full detail. Old view variants are discarded after model edits to
avoid creating an extra drawing-snapshot history. Active geometry remains valid after trimming.

Snapping now compiles bounded block and dimension geometry, including nested transforms and
stored dimension blocks. Each exact probe still reapplies the original per-leaf pruning and
candidate order, and each query retains the shared expansion budget. Intersections, nearest,
perpendicular, tangent, deferred snaps and tracking remain cursor/base dependent. Drawing or
symbol/style changes invalidate compiled data; oversized entries use the original search.

The grid caches its anti-aliased mesh for up to eight views. Grid spacing, snap frame, isometric
mode, theme, DPI and viewport transforms invalidate it. Palette caches reuse unchanged layer
and style lists, selection type groups, common property values and polyline length/area.
Weak entity identities catch preview edits and undo without retaining old drawing snapshots.
Widgets and input handling remain live, and both caches release retained data under pressure.

Regression checks compare cached/uncached snapping and full-detail geometry, reuse after view
changes, invalidation after edits/undo, memory-pressure fallback and direct/retained GPU pixels.
Android device FPS, power and thermal behavior still need measurements on real tablets.

## Lower import and display memory (Android 0.1.4)

Native imports of ASCII DXF and DWG files of at least 4 MiB use a replayable tag reader.
DWG conversion writes to a bounded, host-private temporary DXF rather than retaining its full
text and tag vector alongside the decoded CAD objects. Symbol tables, layouts, object metadata
and foreign extras are retained; graphical entity sequences are mapped directly into COW stores.
The conversion document is released before mapping. Work files are deleted on exit from the
import, and old abandoned work files are cleaned up on startup. A full disk returns an import
error. The original bytes and acadrust's decoded DWG database still need memory; this is not
a direct DWG-to-drawing mapper or an out-of-core document database. Small files, binary DXF
and WebAssembly retain their existing readers.

Tablet model-space scenes split sparsely visible immutable source chunks into ordered groups
of 128 entities. Mostly visible chunks retain their original large batches, preserving overview
draw-call counts; entirely rejected fine groups need no display part.
A budgeted spatial directory rejects safely bounded entities before tessellation. Text,
dimensions, wipeouts, annotative objects and uncertain bounds retain conservative handling.
Blocks with compatible finite contents have transformed bounds, including rotated arrays and
negative scales. Source order is never spatially sorted. Existing cached zoom/view variants
remain reusable, and edits or style changes invalidate affected geometry.

Ordinary GPU geometry pages can encode display positions as two normalized 16-bit integers
instead of two 32-bit floats. Premultiplied colour remains unchanged: payload is 8 rather than
12 bytes per vertex, plus a 24-byte transform per page. Each page checks quantization and
floating-point error against an additional display allowance of about 1/16 pixel at build time;
unrepresentable or precision-sensitive pages retain the original format. Bounds and CPU
geometry stay unquantized. Shared block/glyph templates keep their existing instanced format.

Consecutive runs of at least four eligible small, continuous, thin circles/arcs use analytic
GPU instances (36 bytes per curve), retaining their position among line batches and wipeout
groups. The shader evaluates the ring and arc endpoints. Large curves, patterned or wide
strokes, unsupported transforms, isolated curves and precision-sensitive placements keep the
segment path. Restricting quad coverage to small symbols avoids excessive fragment shading.
Exact drawing coordinates, snapping, software rendering and all exports remain unchanged.

Validation compares streamed and ordinary imports, rendered spatial selection before and after
pans/edits, compact GPU buffers and analytic curves, including shader translation for GLES 3.0.
These changes have not established a device-specific RAM, battery or GPU-time reduction; test
representative drawings on the target tablet before choosing a cache allowance.
