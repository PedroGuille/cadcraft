//! Conservative retained-memory accounting for COW drawing snapshots.
//!
//! Allocation identities, rather than entity handles or serialized drawing sizes, determine
//! sharing. The live and saved drawings can be visited first to measure only history's extra
//! storage. Variable entity payloads are measured once per immutable allocation without
//! creating a JSON buffer; the allowance includes headroom for container spare capacity.

use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::mem::size_of;
use std::sync::{Arc, Weak};

use crate::{Drawing, Entity};

#[derive(Debug, Default)]
pub struct MemoryLedger {
    entities: HashMap<usize, (Weak<Entity>, usize)>,
    drawings: HashMap<usize, (Weak<Drawing>, usize)>,
}

// A memoizer is not snapshot state. Cloning UNDO settings (e.g. UNDO Control) must not copy
// a potentially large entity-size index onto the command's hot path.
impl Clone for MemoryLedger {
    fn clone(&self) -> Self {
        Self::default()
    }
}

impl MemoryLedger {
    pub fn prune(&mut self) {
        self.entities.retain(|_, (weak, _)| weak.strong_count() > 0);
        self.drawings.retain(|_, (weak, _)| weak.strong_count() > 0);
        if self.entities.len() < self.entities.capacity() / 4 {
            self.entities.shrink_to_fit();
        }
        if self.drawings.len() < self.drawings.capacity() / 4 {
            self.drawings.shrink_to_fit();
        }
    }
    pub fn begin(&mut self) -> MemoryUsage<'_> {
        self.prune();
        MemoryUsage { ledger: self, seen: HashSet::new(), baselines: Vec::new(), bytes: 0 }
    }
}

pub struct MemoryUsage<'a> {
    ledger: &'a mut MemoryLedger,
    seen: HashSet<usize>,
    baselines: Vec<Arc<Drawing>>,
    pub bytes: usize,
}

impl MemoryUsage<'_> {
    /// Exclude storage already required by a live or saved drawing. Only visit its chunk and
    /// index identities now; individual entities are compared lazily in changed chunks. Thus
    /// one small edit does not rescan a million unchanged entities to price its undo step.
    pub fn exclude(&mut self, drawing: &Arc<Drawing>) {
        if !self.allocation(drawing, 0) {
            return;
        }
        drawing.model.retained_memory(self, false);
        for layout in &drawing.layouts {
            layout.entities.retained_memory(self, false);
        }
        for block in drawing.blocks.values() {
            if self.allocation(block, 0) {
                block.entities.retained_memory(self, false);
            }
        }
        self.allocation(&drawing.foreign, 0);
        self.baselines.push(drawing.clone());
    }
    pub(crate) fn add(&mut self, bytes: usize) {
        self.bytes = self.bytes.saturating_add(bytes);
    }

    pub(crate) fn allocation<T>(&mut self, value: &Arc<T>, bytes: usize) -> bool {
        if !self.seen.insert(Arc::as_ptr(value) as usize) {
            return false;
        }
        self.add(bytes.saturating_add(2 * size_of::<usize>()));
        true
    }

    pub(crate) fn entity(&mut self, entity: &Arc<Entity>) {
        if self.baselines.iter().any(|d| {
            d.model.get(entity.handle).is_some_and(|e| Arc::ptr_eq(e, entity))
                || d.layouts.iter().any(|l| l.entities.get(entity.handle).is_some_and(|e| Arc::ptr_eq(e, entity)))
                || d.blocks.values().any(|b| b.entities.get(entity.handle).is_some_and(|e| Arc::ptr_eq(e, entity)))
        }) {
            return;
        }
        // Sharing a whole chunk does not increment its entities' Arc counts. An entity with
        // one owner can only be reached through that already-deduplicated chunk; avoid a
        // million-entry address table for the untouched part of a large drawing.
        if Arc::strong_count(entity) > 1 {
            if !self.allocation(entity, 0) {
                return;
            }
        } else {
            self.add(2 * size_of::<usize>());
        }
        use crate::EntityKind as K;
        let bytes = match &entity.kind {
            K::Line(_)
            | K::Point(_)
            | K::Circle(_)
            | K::Arc(_)
            | K::Ellipse(_)
            | K::Ray(_)
            | K::XLine(_)
            | K::Solid(_)
            | K::Trace(_)
            | K::Face3d(_)
            | K::Helix(_)
            | K::LwPolyline(_)
            | K::Polyline3d(_)
            | K::Spline(_) => entity_bytes(entity),
            _ => self.ledger.entities.entry(Arc::as_ptr(entity) as usize).or_insert_with(|| (Arc::downgrade(entity), entity_bytes(entity))).1,
        };
        self.add(bytes);
    }

    pub fn drawing(&mut self, drawing: &Arc<Drawing>) {
        if !self.allocation(drawing, 0) {
            return;
        }
        let entry = self.ledger.drawings.entry(Arc::as_ptr(drawing) as usize).or_insert_with(|| (Arc::downgrade(drawing), metadata(drawing)));
        let bytes = entry.1;
        self.add(bytes);
        drawing.model.retained_memory(self, true);
        for layout in &drawing.layouts {
            layout.entities.retained_memory(self, true);
        }
        for block in drawing.blocks.values() {
            if self.allocation(block, 0) {
                self.add(estimate(block.as_ref()));
                block.entities.retained_memory(self, true);
            }
        }
        // Foreign file records are immutable and shared by ordinary edits. Their storage is
        // part of both the live and saved baselines, never charged once per undo step.
        if self.allocation(&drawing.foreign, size_of::<crate::foreign::Foreign>()) {
            let foreign = &drawing.foreign;
            self.add(estimate(&foreign.classes) + estimate(&foreign.dictionary) + estimate(&foreign.targets) + estimate(&foreign.data_store));
            for object in &foreign.objects {
                self.add(size_of::<crate::foreign::ForeignObject>() + object.kind.capacity() + estimate(&object.tags));
            }
            for extra in foreign.entities.values().chain(foreign.records.values()) {
                self.add(size_of::<crate::foreign::ForeignExtra>() + estimate(&extra.reactors) + estimate(&extra.xdata));
            }
        }
    }
}

fn entity_bytes(e: &Entity) -> usize {
    use crate::EntityKind as K;
    let c = &e.common;
    let common = c.layer.capacity()
        + c.linetype.capacity()
        + c.plot_style.capacity()
        + estimate(&c.anno_scales)
        + c.xdata.capacity() * size_of::<crate::RawTag>()
        + c.xdata.iter().map(|tag| tag.value.capacity()).sum::<usize>();
    // Most large drawings consist of these primitives. Their exact inline size and owned
    // property strings can be counted directly, without running a serializer per line.
    let extra = match &e.kind {
        K::Line(_)
        | K::Point(_)
        | K::Circle(_)
        | K::Arc(_)
        | K::Ellipse(_)
        | K::Ray(_)
        | K::XLine(_)
        | K::Solid(_)
        | K::Trace(_)
        | K::Face3d(_)
        | K::Helix(_) => 0,
        K::LwPolyline(p) => p.vertices.capacity() * size_of::<cadcraft_geom::PolyVertex>(),
        K::Polyline3d(p) => p.points.capacity() * size_of::<cadcraft_geom::Vec3>(),
        K::Spline(p) => {
            (p.control.capacity() + p.fit.capacity()) * size_of::<cadcraft_geom::Vec2>()
                + p.knots.capacity() * size_of::<f64>()
                + p.weights.capacity() * size_of::<f64>()
        }
        _ => estimate(&e.kind),
    };
    size_of::<Entity>().saturating_add(common).saturating_add(extra)
}

#[derive(Default)]
struct Count(usize);
impl Write for Count {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// An estimate, not allocator telemetry: four times the wire payload covers binary numeric
// arrays, strings, tree nodes and typical Vec growth. No temporary serialized copy is kept.
fn estimate<T: serde::Serialize>(value: &T) -> usize {
    let mut count = Count::default();
    let _ = serde_json::to_writer(&mut count, value);
    size_of::<T>().saturating_add(count.0.saturating_mul(4))
}

fn metadata(d: &Drawing) -> usize {
    let mut bytes = size_of::<Drawing>();
    macro_rules! fields {
        ($($field:ident),* $(,)?) => { $(bytes = bytes.saturating_add(estimate(&d.$field));)* };
    }
    fields!(
        header,
        layers,
        linetypes,
        text_styles,
        dim_styles,
        mleader_styles,
        table_styles,
        mline_styles,
        layouts,
        views,
        vports,
        ucss,
        layer_states,
        groups,
        plot_style_tables,
        constraints,
        parametric,
        scales,
        hidden_objects,
        read_notes,
        skipped_records
    );
    for name in d.blocks.keys() {
        bytes = bytes.saturating_add(name.capacity() + 2 * size_of::<(String, Arc<crate::Block>)>());
    }
    if let Some(edit) = &d.refedit {
        bytes = bytes.saturating_add(
            edit.block.capacity()
                + edit.items.capacity() * size_of::<crate::WorkItem>()
                + edit.ghosts.capacity() * size_of::<(crate::Handle, crate::Handle)>()
                + estimate(&edit.released),
        );
    }
    bytes
}
