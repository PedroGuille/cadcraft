//! Ordinary entity tessellation survives edits to neighbouring entities in the COW source chunk.
use super::*;
use std::sync::Weak;

struct Entry {
    source: Weak<Entity>,
    options: Options,
    list: Arc<DisplayList>,
    bytes: usize,
}

#[derive(Default)]
pub(super) struct Cache {
    entries: HashMap<usize, Entry>,
    order: VecDeque<usize>,
    pub bytes: usize,
    limit: usize,
    #[cfg(not(target_arch = "wasm32"))]
    disk: Option<super::disk::Disk>,
}

impl Cache {
    pub fn configure_disk(&mut self, drawing: &Drawing) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            if self.limit >= 1 << 20 {
                self.disk = super::disk::Disk::open(drawing);
            }
        }
        #[cfg(target_arch = "wasm32")]
        let _ = drawing;
    }

    pub fn retained(&self) -> usize {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.bytes.saturating_add(self.disk.as_ref().map_or(0, super::disk::Disk::bytes))
        }
        #[cfg(target_arch = "wasm32")]
        {
            self.bytes
        }
    }

    pub fn flush(&mut self) {
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(disk) = self.disk.as_mut() {
            disk.flush();
        }
        self.set_budget(self.limit);
    }
    pub fn set_budget(&mut self, bytes: usize) {
        self.limit = bytes.min(16 << 20);
        #[cfg(not(target_arch = "wasm32"))]
        if self.disk.as_ref().is_some_and(|d| d.bytes() > self.limit / 2) {
            self.disk = None;
        }
        while self.retained() > self.limit && !self.entries.is_empty() {
            self.evict();
        }
        if self.limit == 0 {
            self.entries.shrink_to_fit();
            self.order.shrink_to_fit();
        }
    }

    pub fn get(&mut self, entity: &Arc<Entity>, options: &Options, drawing: &Drawing) -> Option<Arc<DisplayList>> {
        if let Some(entry) = self.entries.get(&(Arc::as_ptr(entity) as usize))
            && entry.source.ptr_eq(&Arc::downgrade(entity))
            && same_detail(&entry.options, options)
        {
            return Some(entry.list.clone());
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(disk) = self.disk.as_mut() {
            let key = super::disk::Disk::key(drawing, entity, options)?;
            if let Some(list) = disk.get(&key) {
                let answer = Arc::new(list);
                self.store(entity, options, answer.clone(), drawing);
                return Some(answer);
            }
        }
        #[cfg(target_arch = "wasm32")]
        let _ = drawing;
        None
    }

    pub fn insert(&mut self, entity: &Arc<Entity>, options: &Options, list: DisplayList, drawing: &Drawing) {
        self.store(entity, options, Arc::new(list), drawing);
    }

    fn store(&mut self, entity: &Arc<Entity>, options: &Options, list: Arc<DisplayList>, drawing: &Drawing) {
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(disk) = self.disk.as_mut()
            && let Some(key) = super::disk::Disk::key(drawing, entity, options)
        {
            disk.insert(&key, &list);
        }
        #[cfg(target_arch = "wasm32")]
        let _ = drawing;
        let bytes = super::bytes(&list).saturating_add(std::mem::size_of::<Entry>());
        if bytes > self.limit || bytes > 256 << 10 {
            return;
        }
        let key = Arc::as_ptr(entity) as usize;
        if let Some(old) = self.entries.remove(&key) {
            self.bytes = self.bytes.saturating_sub(old.bytes);
            self.order.retain(|old| *old != key);
        }
        while !self.entries.is_empty() && (self.entries.len() >= 4096 || self.retained().saturating_add(bytes) > self.limit) {
            self.evict();
        }
        if self.retained().saturating_add(bytes) > self.limit {
            return;
        }
        self.bytes = self.bytes.saturating_add(bytes);
        self.entries.insert(key, Entry { source: Arc::downgrade(entity), options: Options { cancel: None, ..options.clone() }, list, bytes });
        self.order.push_back(key);
    }

    fn evict(&mut self) {
        if let Some(key) = self.order.pop_front()
            && let Some(entry) = self.entries.remove(&key)
        {
            self.bytes = self.bytes.saturating_sub(entry.bytes);
        }
    }
}

pub(super) fn eligible(e: &Entity) -> bool {
    !cadcraft_doc::is_annotative(e)
        && matches!(
            e.kind,
            EntityKind::Line(_)
                | EntityKind::Circle(_)
                | EntityKind::Arc(_)
                | EntityKind::Ellipse(_)
                | EntityKind::LwPolyline(_)
                | EntityKind::Spline(_)
                | EntityKind::Point(_)
                | EntityKind::Solid(_)
                | EntityKind::Trace(_)
        )
}

pub(super) fn append(dst: &mut DisplayList, src: &DisplayList) {
    let prims = dst.prims.len();
    let (verts, tris) = (dst.verts.len() as u32, dst.tris.len() as u32);
    dst.prims.extend(
        src.prims
            .iter()
            .map(|p| crate::DPrim { start: p.start + if matches!(p.kind, crate::Kind::Tris | crate::Kind::Mask) { tris } else { verts }, ..*p }),
    );
    dst.verts.extend_from_slice(&src.verts);
    dst.tris.extend_from_slice(&src.tris);
    dst.analytic.extend(src.analytic.iter().map(|hint| crate::ArcHint { prim: hint.prim + prims, ..*hint }));
    dst.bounds = dst.bounds.union(&src.bounds);
}

pub(super) fn slice(src: &DisplayList, first: usize) -> Option<DisplayList> {
    let size = src.prims.iter().skip(first).fold(0usize, |n, p| n.saturating_add(p.len as usize * 16 + std::mem::size_of::<crate::DPrim>()));
    if size > 240 << 10 {
        return None;
    }
    let mut list = DisplayList::default();
    for (at, source) in src.prims.iter().enumerate().skip(first) {
        let target = if matches!(source.kind, crate::Kind::Tris | crate::Kind::Mask) { &mut list.tris } else { &mut list.verts };
        let start = target.len() as u32;
        target.extend_from_slice(src.points(source));
        list.prims.push(crate::DPrim { start, ..*source });
        if let Some(hint) = src.analytic.iter().find(|h| h.prim == at) {
            list.analytic.push(crate::ArcHint { prim: at - first, ..*hint });
        }
    }
    list.bounds = Bounds2::from_points(list.verts.iter().chain(&list.tris).copied());
    Some(list)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn entity_reuse_requires_same_immutable_source_detail_and_budget() {
        let mut d = Drawing::new_imperial();
        let h =
            d.add(&Space::Model, Default::default(), EntityKind::Point(cadcraft_doc::Point { p: cadcraft_geom::Vec3::ZERO, angle: 0.0 })).unwrap();
        let e = d.entity(h).unwrap().clone();
        let mut cache = Cache::default();
        cache.set_budget(4096);
        let options = Options::default();
        cache.insert(&e, &options, DisplayList::default(), &d);
        assert!(cache.get(&e, &options, &d).is_some());
        assert!(cache.get(&Arc::new((*e).clone()), &options, &d).is_none());
        assert!(cache.get(&e, &Options { view_height: 400.0, ..options.clone() }, &d).is_none());
        cache.set_budget(0);
        assert!(cache.get(&e, &options, &d).is_none());
    }
}
