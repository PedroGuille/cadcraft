//! Spatial metadata for immutable placements and ordered draw operations. Geometry stays ordered.
use super::*;

const LEAF: usize = 8;
const MAX_ITEMS: usize = 524_288;

struct Node {
    bounds: Bounds2,
    children: Option<(usize, usize)>,
    items: std::ops::Range<usize>,
}

pub(crate) struct Index {
    nodes: Vec<Node>,
    ids: Vec<usize>,
    uncertain: Vec<usize>,
}

impl Index {
    pub fn new(bounds: &[Bounds2]) -> Option<Self> {
        if bounds.len() < 32 || bounds.len() > MAX_ITEMS {
            return None;
        }
        let (mut ids, uncertain): (Vec<_>, Vec<_>) = (0..bounds.len()).partition(|&i| finite(bounds[i]));
        let mut index = Self { nodes: Vec::new(), ids: Vec::with_capacity(ids.len()), uncertain };
        if !ids.is_empty() {
            index.build(&mut ids, bounds);
        }
        Some(index)
    }

    fn build(&mut self, ids: &mut [usize], bounds: &[Bounds2]) -> usize {
        let area = ids.iter().fold(Bounds2::EMPTY, |b, &id| b.union(&bounds[id]));
        let at = self.nodes.len();
        self.nodes.push(Node { bounds: area, children: None, items: 0..0 });
        if ids.len() <= LEAF {
            let first = self.ids.len();
            self.ids.extend_from_slice(ids);
            if let Some(node) = self.nodes.get_mut(at) {
                node.items = first..self.ids.len();
            }
        } else {
            let horizontal = area.width() >= area.height();
            let coordinate = |id: usize| {
                let b = bounds[id];
                if horizontal { b.min.x * 0.5 + b.max.x * 0.5 } else { b.min.y * 0.5 + b.max.y * 0.5 }
            };
            let mid = ids.len() / 2;
            ids.select_nth_unstable_by(mid, |&a, &b| coordinate(a).total_cmp(&coordinate(b)));
            let (a, b) = ids.split_at_mut(mid);
            let left = self.build(a, bounds);
            let right = self.build(b, bounds);
            if let Some(node) = self.nodes.get_mut(at) {
                node.children = Some((left, right));
            }
        }
        at
    }

    pub fn bytes(&self) -> usize {
        self.nodes.capacity() * std::mem::size_of::<Node>() + (self.ids.capacity() + self.uncertain.capacity()) * std::mem::size_of::<usize>()
    }

    pub fn indices(&self, bounds: &[Bounds2], view: &Bounds2) -> Vec<usize> {
        if !finite(*view) {
            return (0..bounds.len()).collect();
        }
        let mut out = self.uncertain.clone();
        if !self.nodes.is_empty() {
            self.visit(0, bounds, view, &mut out);
        }
        out.sort_unstable();
        out
    }

    fn visit(&self, at: usize, bounds: &[Bounds2], view: &Bounds2, out: &mut Vec<usize>) {
        // Balanced construction from at most MAX_ITEMS records bounds depth to 17 edges.
        let Some(node) = self.nodes.get(at) else { return };
        if !node.bounds.intersects(view) {
            return;
        }
        if let Some((left, right)) = node.children {
            self.visit(left, bounds, view, out);
            self.visit(right, bounds, view, out);
        } else if let Some(ids) = self.ids.get(node.items.clone()) {
            out.extend(ids.iter().copied().filter(|&id| bounds.get(id).is_none_or(|b| b.intersects(view))));
        }
    }
}

fn finite(b: Bounds2) -> bool {
    !b.is_empty() && b.min.is_finite() && b.max.is_finite()
}

pub(crate) fn ranges(bounds: &[Bounds2], index: Option<&Index>, view: Option<Bounds2>) -> Vec<std::ops::Range<u32>> {
    let ids: Vec<_> = match (index, view.filter(|v| finite(*v))) {
        (Some(index), Some(view)) => index.indices(bounds, &view),
        (_, Some(view)) => bounds.iter().enumerate().filter(|(_, b)| !finite(**b) || b.intersects(&view)).map(|(i, _)| i).collect(),
        _ => (0..bounds.len()).collect(),
    };
    let mut out: Vec<std::ops::Range<u32>> = Vec::new();
    for id in ids {
        let Ok(id) = u32::try_from(id) else { break };
        if let Some(last) = out.last_mut().filter(|last| last.end == id) {
            last.end = id + 1;
        } else {
            out.push(id..id + 1);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn spatial_placements_match_linear_visibility_and_keep_unknowns_and_source_order() {
        let mut bounds: Vec<_> = (0..4096)
            .map(|i| {
                let x = f64::from((i * 997) % 4096);
                Bounds2::new(Vec2::new(x, 0.0), Vec2::new(x + 0.5, 1.0))
            })
            .collect();
        bounds[57] = Bounds2::EMPTY;
        bounds[71] = Bounds2 { min: Vec2::new(f64::NAN, 0.0), max: Vec2::new(f64::NAN, 0.0) };
        let index = Index::new(&bounds).unwrap();
        for x in [-20.0, 0.0, 100.0, 2000.0, 5000.0] {
            let view = Some(Bounds2::new(Vec2::new(x, -1.0), Vec2::new(x + 50.0, 2.0)));
            assert_eq!(ranges(&bounds, Some(&index), view), ranges(&bounds, None, view));
        }
        assert_eq!(ranges(&bounds, Some(&index), None), vec![0..4096]);
    }
}
