//! A spatial directory over immutable source chunks. Leaves refer to source positions rather
//! than reordering entities: colours, transparency and wipeout barriers keep CAD draw order.

use std::collections::HashMap;
use std::sync::Arc;

use cadcraft_doc::{Drawing, Entity, EntityKind, MAX_BLOCK_DEPTH};
use cadcraft_geom::{Bounds2, Vec2};

#[derive(Clone, Default)]
pub(super) struct Cache {
    chunks: HashMap<usize, Arc<Index>>,
    blocks: HashMap<(String, usize), Option<Bounds2>>,
}

struct Node {
    bounds: Bounds2,
    items: std::ops::Range<usize>,
    children: Option<(usize, usize)>,
}

struct Index {
    bounds: Vec<Option<Bounds2>>,
    ordered: Vec<usize>,
    nodes: Vec<Node>,
    unknown: Vec<usize>,
}

impl Index {
    fn new(bounds: Vec<Option<Bounds2>>) -> Self {
        let ordered = bounds.iter().enumerate().filter_map(|(i, b)| b.map(|_| i)).collect();
        let unknown = bounds.iter().enumerate().filter_map(|(i, b)| b.is_none().then_some(i)).collect();
        let mut index = Self { bounds, ordered, nodes: Vec::new(), unknown };
        if !index.ordered.is_empty() {
            index.split(0..index.ordered.len());
        }
        index
    }

    fn split(&mut self, range: std::ops::Range<usize>) -> usize {
        let bounds = self
            .ordered
            .get(range.clone())
            .unwrap_or(&[])
            .iter()
            .filter_map(|i| self.bounds.get(*i).copied().flatten())
            .fold(Bounds2::EMPTY, |a, b| a.union(&b));
        let at = self.nodes.len();
        self.nodes.push(Node { bounds, items: range.clone(), children: None });
        if range.len() > 16 {
            let x = bounds.width() >= bounds.height();
            if let Some(items) = self.ordered.get_mut(range.clone()) {
                items.sort_unstable_by(|a, b| {
                    let center = |i: &usize| self.bounds.get(*i).copied().flatten().map_or(Vec2::ZERO, |b| b.center());
                    let (a, b) = (center(a), center(b));
                    if x { a.x.total_cmp(&b.x) } else { a.y.total_cmp(&b.y) }
                });
            }
            let middle = range.start + range.len() / 2;
            let children = (self.split(range.start..middle), self.split(middle..range.end));
            if let Some(node) = self.nodes.get_mut(at) {
                node.children = Some(children);
            }
        }
        at
    }

    fn mask(&self, view: Bounds2) -> Vec<bool> {
        let mut mask = vec![false; self.bounds.len()];
        for i in &self.unknown {
            if let Some(value) = mask.get_mut(*i) {
                *value = true;
            }
        }
        let mut pending = if self.nodes.is_empty() { Vec::new() } else { vec![0] };
        while let Some(at) = pending.pop() {
            let Some(node) = self.nodes.get(at).filter(|n| n.bounds.intersects(&view)) else { continue };
            if let Some((a, b)) = node.children {
                pending.extend([a, b]);
            } else {
                for i in self.ordered.get(node.items.clone()).unwrap_or(&[]) {
                    if self.bounds.get(*i).copied().flatten().is_some_and(|b| b.intersects(&view))
                        && let Some(value) = mask.get_mut(*i)
                    {
                        *value = true;
                    }
                }
            }
        }
        mask
    }

    fn bytes(&self) -> usize {
        self.bounds.capacity() * std::mem::size_of::<Option<Bounds2>>()
            + (self.ordered.capacity() + self.unknown.capacity()) * std::mem::size_of::<usize>()
            + self.nodes.capacity() * std::mem::size_of::<Node>()
    }
}

impl Cache {
    pub fn bytes(&self) -> usize {
        self.chunks.values().map(|index| index.bytes()).sum::<usize>() + self.blocks.iter().map(|((name, _), _)| name.capacity() + 64).sum::<usize>()
    }

    pub fn trim(&mut self, limit: usize) {
        if self.bytes() > limit {
            self.chunks.clear();
            self.blocks.clear();
        }
    }

    pub fn masks(&mut self, d: &Drawing, view: Option<Bounds2>, tolerance: f64, limit: usize) -> HashMap<usize, Vec<bool>> {
        let Some(view) = view.filter(|v| !v.is_empty() && v.min.is_finite() && v.max.is_finite()) else { return HashMap::new() };
        let keys: std::collections::HashSet<_> = d.model.chunk_slices().map(|(key, _)| key).collect();
        self.chunks.retain(|key, _| keys.contains(key));
        self.trim(limit);
        let magnitude = view.min.x.abs().max(view.min.y.abs()).max(view.max.x.abs()).max(view.max.y.abs());
        let view = view.expand(tolerance.abs() * 64.0 + magnitude * f64::EPSILON * 128.0);
        let mut masks = HashMap::new();
        let complex = d.linetypes.iter().any(|lt| lt.pattern.iter().any(|p| p.text.is_some() || p.shape.is_some()));
        for (key, entities) in d.model.chunk_slices() {
            let index = if let Some(index) = self.chunks.get(&key) {
                index.clone()
            } else {
                let index = Arc::new(Index::new(entities.iter().map(|e| bounds(d, e, 0, &mut self.blocks, complex)).collect()));
                if self.bytes().saturating_add(index.bytes()) <= limit {
                    self.chunks.insert(key, index.clone());
                }
                index
            };
            masks.insert(key, index.mask(view));
        }
        masks
    }
}

fn bounds(d: &Drawing, e: &Entity, depth: usize, memo: &mut HashMap<(String, usize), Option<Bounds2>>, complex: bool) -> Option<Bounds2> {
    if cadcraft_doc::is_annotative(e) || depth >= MAX_BLOCK_DEPTH {
        return None;
    }
    // Complex linetypes can extend beyond the centre line by text or shape geometry.
    if complex && e.common.linetype != "Continuous" {
        return None;
    }
    let value = match &e.kind {
        EntityKind::Line(_)
        | EntityKind::Circle(_)
        | EntityKind::Arc(_)
        | EntityKind::Ellipse(_)
        | EntityKind::Spline(_)
        | EntityKind::LwPolyline(_)
        | EntityKind::Polyline3d(_)
        | EntityKind::Solid(_)
        | EntityKind::Trace(_)
        | EntityKind::Face3d(_)
        | EntityKind::Hatch(_) => cadcraft_doc::entity_bounds(d, e, depth),
        EntityKind::Insert(insert) if insert.attribs.is_empty() => {
            let block = d.block(&insert.block)?;
            let key = (block.name.clone(), depth);
            let local = if let Some(value) = memo.get(&key) {
                *value
            } else {
                let mut value = Some(Bounds2::EMPTY);
                for entity in block.entities.iter().filter(|e| !matches!(e.kind, EntityKind::AttDef(_))) {
                    value = value.zip(bounds(d, entity, depth + 1, memo, complex)).map(|(a, b)| a.union(&b));
                    if value.is_none() {
                        break;
                    }
                }
                if memo.len() < 4096 {
                    memo.insert(key, value);
                }
                value
            }?;
            let transform = insert.transform(block.base.xy());
            let a = Vec2::new(insert.col_spacing * f64::from(insert.cols.clamp(1, 10_000) - 1), 0.0).rotate(insert.rotation);
            let b = Vec2::new(0.0, insert.row_spacing * f64::from(insert.rows.clamp(1, 10_000) - 1)).rotate(insert.rotation);
            Bounds2::from_points(local.corners().into_iter().flat_map(|p| [Vec2::ZERO, a, b, a + b].map(|offset| transform.apply(p) + offset)))
        }
        // Text, dimension overrides, masks, infinite lines and unsupported geometry keep the
        // full renderer. Approximate text extents are never used to hide real drawing content.
        _ => return None,
    };
    (!value.is_empty() && value.min.is_finite() && value.max.is_finite()).then_some(value)
}
