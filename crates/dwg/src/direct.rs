//! Typed record lowering. The decoder remains an unmodified, isolated dependency.
//! The entity pass consumes its native allocations instead of retaining two complete models.
use std::cell::Cell;
use std::io::Cursor;
use std::panic::AssertUnwindSafe;

use acadrust::entities::EntityType;
use acadrust::io::dxf::{DxfStreamWriter, SectionWriter};
use acadrust::{CadDocument, DxfError, Handle};

use super::Limits;

/// A borrowed decoded field. Coordinates never pass through decimal text.
#[derive(Clone, Copy, Debug)]
pub enum Field<'a> {
    Text(&'a str),
    Xrecord(&'a str),
    Real(f64),
    Int(i64),
    Bool(bool),
    Handle(u64),
    Binary(&'a [u8]),
}

/// Decoded DWG and compatibility metadata. Dependency-specific types stay private.
pub struct DecodedDrawing {
    document: CadDocument,
    notes: Vec<String>,
    limits: Limits,
    start: u64,
    seed: u64,
}

impl DecodedDrawing {
    /// Decode once, retaining failsafe diagnostics and checking the input size before decoding.
    pub fn read(bytes: &[u8], limits: Limits) -> Result<Self, String> {
        if bytes.len() > limits.max_dwg_bytes {
            return Err(format!(
                "DWG: the file is {}, larger than the {} CADCraft opens",
                super::mb(bytes.len() as u64),
                super::mb(limits.max_dwg_bytes as u64)
            ));
        }
        guarded(|| {
            let outcome = acadrust::DwgReader::from_stream_with_options(Cursor::new(bytes), acadrust::DwgReadOptions::failsafe())
                .read_with_stats()
                .map_err(|e| format!("DWG: {e}"))?;
            if !outcome.stats.record_stream_read {
                let why = outcome
                    .stats
                    .diagnostics
                    .first()
                    .map(|d| d.message.trim_start_matches("Failsafe: "))
                    .unwrap_or("the file's objects couldn't be read");
                return Err(format!("DWG: {why}"));
            }
            let mut notes = super::native::read_notes(&outcome.stats);
            let mut document = outcome.document;
            super::native::prepare_read(&mut document, &mut notes);
            let (start, seed) = handles(&document)?;
            Ok(Self { document, notes, limits, start, seed })
        })
    }

    pub fn notes(&self) -> &[String] {
        &self.notes
    }

    pub fn header_overrides(&self) -> Vec<(&'static str, i32, String)> {
        super::native::unwritten_header_vars(&self.document.header).into_iter().collect()
    }

    /// Visit typed metadata in original section/record order. Consumers discard geometry here
    /// and retain only symbol tables, objects, block headers and foreign entity properties.
    pub fn metadata(&self, visit: impl FnMut(i32, Field<'_>) -> Result<(), String>) -> Result<(), String> {
        guarded(|| {
            let mut stream = Fields::new(visit, self.limits);
            let mut writer = SectionWriter::new(&mut stream, self.start, self.seed);
            writer.set_version(self.document.version);
            writer.build_valid_handles(&self.document);
            writer.write_header(&self.document).map_err(error)?;
            writer.write_classes(&self.document).map_err(error)?;
            writer.write_tables(&self.document).map_err(error)?;
            writer.write_blocks(&self.document).map_err(error)?;
            writer.write_entities(&self.document).map_err(error)?;
            writer.write_objects(&self.document).map_err(error)?;
            writer.write_acdsdata().map_err(error)?;
            stream.write_string(0, "EOF").map_err(error)
        })
    }

    /// Consume entities in the same block/model/paper order as the compatibility reader.
    /// Each callback sees one bounded entity sequence (including ATTRIB/VERTEX followers),
    /// delimited by SECTION/ENDSEC. `block` is set for a named block body, including inactive
    /// paper-space blocks. Original owner handles route model/active-paper entities.
    pub fn entities(mut self, mut visit: impl FnMut(Option<&str>, i32, Field<'_>) -> Result<(), String>) -> Result<(), String> {
        guarded(|| {
            let current = std::cell::RefCell::new(None::<String>);
            let enabled = Cell::new(false);
            let mut stream = Fields::new(
                |code, value| {
                    if enabled.get() { visit(current.borrow().as_deref(), code, value) } else { Ok(()) }
                },
                self.limits,
            );
            let mut writer = SectionWriter::new(&mut stream, self.start, self.seed);
            writer.set_version(self.document.version);
            writer.build_valid_handles(&self.document);
            // Null layer handles are allocated before child-entity handles by SectionWriter.
            writer.write_tables(&self.document).map_err(error)?;
            let mut plans = Vec::new();
            for block in self.document.block_records.iter_mut() {
                let priority = if block.name.eq_ignore_ascii_case("*Model_Space") {
                    1
                } else if block.name.eq_ignore_ascii_case("*Paper_Space") {
                    2
                } else {
                    0
                };
                plans.push((priority, block.name.clone(), block.handle, block.is_paper_space(), std::mem::take(&mut block.entity_handles)));
            }
            plans.sort_by_key(|p| p.0); // Stable: preserve the file's order within BLOCKS.
            let mut single = CadDocument::new();
            let mut emitted = 0usize;
            for (priority, name, owner, paper, handles) in plans {
                *current.borrow_mut() = (priority == 0).then_some(name);
                // Reserve explicit viewport IDs before assigning missing ones, per paper block.
                let mut used: std::collections::HashSet<_> = handles
                    .iter()
                    .filter_map(|h| match self.document.get_entity(*h) {
                        Some(EntityType::Viewport(v)) if v.id > 0 => Some(v.id),
                        _ => None,
                    })
                    .collect();
                let mut next = 1i16;
                for handle in handles {
                    let Some(mut entity) = self.document.remove_entity_arc(handle) else { continue };
                    if paper
                        && let EntityType::Viewport(viewport) = entity.as_ref()
                        && viewport.id == 0
                    {
                        while used.contains(&next) && next < i16::MAX {
                            next += 1;
                        }
                        let id = if used.insert(next) { next } else { 0 };
                        if let EntityType::Viewport(v) = std::sync::Arc::make_mut(&mut entity) {
                            v.id = id;
                        }
                    }
                    let slot = if paper { "*Paper_Space" } else { "*Model_Space" };
                    if let Some(block) = single.block_records.get_mut(slot) {
                        block.handle = owner;
                        block.entity_handles.clear();
                        block.entity_handles.push(handle);
                    }
                    if single.restore_entity_arc(entity).is_none() {
                        return Err("DWG: invalid entity identity during direct import".into());
                    }
                    enabled.set(true);
                    let result = writer.write_entities(&single).map_err(error);
                    enabled.set(false);
                    single.remove_entity_arc(handle);
                    result?;
                    emitted = emitted.saturating_add(1);
                    if emitted > MAX_FIELDS {
                        return Err("DWG: too many entities".into());
                    }
                }
                // Once a body is mapped, no native entity payload for it stays in memory.
            }
            Ok(())
        })
    }
}

const MAX_FIELDS: usize = 50_000_000;
const MAX_VALUE: usize = 1 << 20;
const MAX_HANDLE: u64 = 1 << 48;

fn guarded<T>(f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    std::panic::catch_unwind(AssertUnwindSafe(f)).map_err(|_| "DWG: direct import failed on this file".to_string())?
}
fn error(e: DxfError) -> String {
    format!("DWG: {e}")
}

struct Fields<F> {
    visit: F,
    count: usize,
    bytes: usize,
    limit: usize,
}
impl<F> Fields<F>
where
    F: FnMut(i32, Field<'_>) -> Result<(), String>,
{
    fn new(visit: F, limits: Limits) -> Self {
        Self { visit, count: 0, bytes: 0, limit: limits.max_dxf_bytes }
    }
    fn field(&mut self, code: i32, value: Field<'_>) -> acadrust::Result<()> {
        let payload = match value {
            Field::Text(s) | Field::Xrecord(s) => s.len(),
            Field::Binary(b) => b.len(),
            _ => 8,
        };
        self.count = self.count.saturating_add(1);
        self.bytes = self.bytes.saturating_add(payload.saturating_add(8));
        if payload > MAX_VALUE || self.count > MAX_FIELDS || self.bytes > self.limit {
            return Err(DxfError::Parse("direct import exceeds the decoded-data limit".into()));
        }
        (self.visit)(code, value).map_err(DxfError::Parse)
    }
}
impl<F> DxfStreamWriter for Fields<F>
where
    F: FnMut(i32, Field<'_>) -> Result<(), String>,
{
    fn write_string(&mut self, code: i32, value: &str) -> acadrust::Result<()> {
        self.field(code, Field::Text(value))
    }
    fn write_xrecord_string(&mut self, code: i32, value: &str) -> acadrust::Result<()> {
        self.field(code, Field::Xrecord(value))
    }
    fn write_byte(&mut self, code: i32, value: u8) -> acadrust::Result<()> {
        self.write_i64(code, i64::from(value))
    }
    fn write_i16(&mut self, code: i32, value: i16) -> acadrust::Result<()> {
        self.write_i64(code, i64::from(value))
    }
    fn write_i32(&mut self, code: i32, value: i32) -> acadrust::Result<()> {
        self.write_i64(code, i64::from(value))
    }
    fn write_i64(&mut self, code: i32, value: i64) -> acadrust::Result<()> {
        self.field(code, Field::Int(value))
    }
    fn write_double(&mut self, code: i32, value: f64) -> acadrust::Result<()> {
        self.field(code, Field::Real(if value.is_finite() { value } else { 0.0 }))
    }
    fn write_bool(&mut self, code: i32, value: bool) -> acadrust::Result<()> {
        self.field(code, Field::Bool(value))
    }
    fn write_handle(&mut self, code: i32, value: Handle) -> acadrust::Result<()> {
        self.field(code, Field::Handle(value.value()))
    }
    fn write_binary(&mut self, code: i32, value: &[u8]) -> acadrust::Result<()> {
        // Binary DXF groups carry at most 127 bytes per record in the compatibility writer.
        for chunk in value.chunks(127) {
            self.field(code, Field::Binary(chunk))?;
        }
        Ok(())
    }
    fn flush(&mut self) -> acadrust::Result<()> {
        Ok(())
    }
}

/// Independent handle accounting for the public section writer: include structural and child
/// handles as well as table/object handles. Reject implausible handles before its unchecked
/// allocator can overflow. Child records generated by the writer receive a disjoint range.
fn handles(d: &CadDocument) -> Result<(u64, u64), String> {
    let mut max = d.next_handle().saturating_sub(1);
    let mut extra = 0u64;
    let mut include = |h: Handle| {
        max = max.max(h.value());
    };
    for e in d.entities() {
        include(e.common().handle);
        match e {
            EntityType::Insert(i) => {
                for a in &i.attributes {
                    include(a.common.handle);
                }
                extra = extra
                    .saturating_add(i.attributes.iter().filter(|a| a.common.handle.is_null()).count() as u64 + u64::from(!i.attributes.is_empty()));
            }
            EntityType::Polyline(p) => extra = extra.saturating_add(p.vertices.len() as u64 + 1),
            EntityType::Polyline2D(p) => {
                let light = p.flags.bits() & !0x81 == 0
                    && p.smooth_surface == acadrust::entities::SmoothSurfaceType::None
                    && p.vertices.iter().all(|v| v.flags.bits() == 0);
                if !light {
                    extra = extra.saturating_add(p.vertices.len() as u64 + 1);
                }
            }
            EntityType::Polyline3D(p) => extra = extra.saturating_add(p.vertices.iter().filter(|v| v.handle.is_null()).count() as u64 + 1),
            EntityType::PolyfaceMesh(p) => {
                extra = extra.saturating_add(
                    p.vertices.iter().filter(|v| v.common.handle.is_null()).count() as u64
                        + p.faces.iter().filter(|v| v.common.handle.is_null()).count() as u64
                        + u64::from(p.seqend_handle.is_none()),
                )
            }
            EntityType::PolygonMesh(p) => extra = extra.saturating_add(p.vertices.iter().filter(|v| v.common.handle.is_null()).count() as u64 + 1),
            _ => {}
        }
    }
    for (handle, object) in &d.objects {
        include(*handle);
        if let acadrust::objects::ObjectType::SortEntitiesTable(table) = object {
            extra = extra.saturating_add(table.len() as u64);
        }
    }
    for b in d.block_records.iter() {
        for h in [b.handle, b.block_entity_handle, b.block_end_handle] {
            include(h);
        }
        for &h in &b.entity_handles {
            include(h);
        }
    }
    macro_rules! tables { ($($table:ident),*) => { $(for r in d.$table.iter() { include(r.handle); })* }; }
    tables!(layers, line_types, text_styles, dim_styles, app_ids, views, vports, ucss);
    extra = extra.saturating_add(d.layers.iter().filter(|l| l.handle.is_null()).count() as u64);
    let start = max.saturating_add(1);
    let seed = start.saturating_add(extra).saturating_add(1);
    if seed > MAX_HANDLE || extra > MAX_FIELDS as u64 {
        return Err("DWG: invalid or excessive handles".into());
    }
    Ok((start, seed))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn drawing() -> Vec<u8> {
        super::super::dxf_to_dwg(b"0\nSECTION\n2\nENTITIES\n0\nLINE\n5\n100\n10\n0\n20\n0\n11\n12.125\n21\n4\n0\nENDSEC\n0\nEOF\n").unwrap()
    }
    #[test]
    fn direct_limits_and_sink_errors_are_propagated() {
        let bytes = drawing();
        assert!(DecodedDrawing::read(&bytes, Limits { max_dwg_bytes: bytes.len() - 1, ..Limits::DEFAULT }).is_err());
        let decoded = DecodedDrawing::read(&bytes, Limits { max_dxf_bytes: 64, ..Limits::DEFAULT }).unwrap();
        assert!(decoded.metadata(|_, _| Ok(())).is_err());
        let decoded = DecodedDrawing::read(&bytes, Limits::DEFAULT).unwrap();
        assert!(decoded.metadata(|_, _| Err("stopped by caller".into())).unwrap_err().contains("stopped by caller"));
        let decoded = DecodedDrawing::read(&bytes, Limits::DEFAULT).unwrap();
        assert!(decoded.entities(|_, _, _| Err("stopped entity mapping".into())).unwrap_err().contains("stopped entity mapping"));
    }
}

#[cfg(test)]
mod allocation_tests {
    use super::*;
    #[test]
    fn native_entity_payloads_are_released_before_the_next_entity() {
        let mut text = String::from("0\nSECTION\n2\nENTITIES\n");
        for handle in 0x100..0x103 {
            text.push_str(&format!("0\nLINE\n5\n{handle:X}\n10\n0\n20\n0\n11\n1\n21\n2\n"));
        }
        text.push_str("0\nENDSEC\n0\nEOF\n");
        let bytes = super::super::dxf_to_dwg(text.as_bytes()).unwrap();
        let decoded = DecodedDrawing::read(&bytes, Limits::DEFAULT).unwrap();
        let allocations: std::collections::HashMap<_, _> = decoded
            .document
            .entities()
            .map(|e| {
                let h = e.common().handle;
                (h.value(), std::sync::Arc::downgrade(&decoded.document.get_entity_arc(h).unwrap()))
            })
            .collect();
        let mut previous = None;
        let mut count = 0;
        decoded
            .entities(|_, code, value| {
                if let (5, Field::Handle(handle)) = (code, value) {
                    if let Some(last) = previous {
                        assert!(allocations[&last].upgrade().is_none());
                    }
                    previous = Some(handle);
                    count += 1;
                }
                Ok(())
            })
            .unwrap();
        assert_eq!(count, 3);
        assert!(allocations.values().all(|weak| weak.upgrade().is_none()));
    }
}
