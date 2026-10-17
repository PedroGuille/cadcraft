//! A small spatial BVH over GPU ranges. Only metadata is spatially sorted: submitted ranges
//! return to their original order, including transparent geometry and wipeout boundaries.
use super::*;

#[derive(Clone, Copy)]
struct Node {
    bounds: [f32; 4],
    left: u16,
    right: u16,
    first: u16,
    count: u16,
}

impl Node {
    fn bounds(&self) -> Bounds2 {
        let [x, y, u, v] = self.bounds.map(f64::from);
        Bounds2::new(Vec2::new(x, y), Vec2::new(u, v))
    }
}

pub(super) struct Index {
    nodes: Vec<Node>,
}

impl Index {
    pub fn new(bytes: &[u8], stride: usize) -> Option<Self> {
        let mut leaves = Vec::new();
        // 24 is divisible by both line and triangle sizes. A 384-record range needs only
        // 31 compact nodes, shared by every frame and every tiled replay.
        for (i, region) in bytes.chunks(24 * stride).enumerate() {
            let mut bounds = Bounds2::EMPTY;
            for vertex in region.chunks_exact(stride) {
                for offset in if stride == INSTANCE_SIZE { &[0, 8][..] } else { &[0][..] } {
                    let pair = vertex.get(*offset..*offset + 8)?;
                    let mut values = pair.as_chunks::<4>().0.iter().map(|v| f32::from_le_bytes(*v));
                    let point = Vec2::new(f64::from(values.next()?), f64::from(values.next()?));
                    if !point.is_finite() {
                        return None;
                    }
                    bounds.add(point);
                }
            }
            leaves.push(Node {
                bounds: [bounds.min.x as f32, bounds.min.y as f32, bounds.max.x as f32, bounds.max.y as f32],
                left: u16::MAX,
                right: u16::MAX,
                first: u16::try_from(i * 24).ok()?,
                count: u16::try_from(region.len() / stride).ok()?,
            });
        }
        if leaves.len() <= 1 || leaves.len() > 64 {
            return None;
        }
        let mut result = Self { nodes: Vec::with_capacity(leaves.len() * 2 - 1) };
        result.build(&mut leaves)?;
        Some(result)
    }

    fn build(&mut self, leaves: &mut [Node]) -> Option<u16> {
        let index = u16::try_from(self.nodes.len()).ok()?;
        let bounds = leaves.iter().fold(Bounds2::EMPTY, |b, n| b.union(&n.bounds()));
        let leaf = *leaves.first()?;
        self.nodes.push(leaf);
        if leaves.len() > 1 {
            let axis = if bounds.width() >= bounds.height() { 0 } else { 1 };
            leaves.sort_unstable_by(|a, b| (a.bounds[axis] + a.bounds[axis + 2]).total_cmp(&(b.bounds[axis] + b.bounds[axis + 2])));
            let mid = leaves.len() / 2;
            let (a, b) = leaves.split_at_mut(mid);
            let left = self.build(a)?;
            let right = self.build(b)?;
            if let Some(node) = self.nodes.get_mut(usize::from(index)) {
                *node = Node {
                    bounds: [bounds.min.x as f32, bounds.min.y as f32, bounds.max.x as f32, bounds.max.y as f32],
                    left,
                    right,
                    first: 0,
                    count: 0,
                };
            }
        }
        Some(index)
    }

    pub fn bytes(&self) -> usize {
        self.nodes.capacity() * std::mem::size_of::<Node>()
    }

    pub fn ranges(&self, view: &Bounds2, clip: std::ops::Range<u32>) -> Vec<std::ops::Range<u32>> {
        let mut result = Vec::new();
        self.visit(0, view, &clip, &mut result);
        result.sort_unstable_by_key(|r| r.start);
        let mut merged: Vec<std::ops::Range<u32>> = Vec::with_capacity(result.len());
        for r in result {
            if let Some(last) = merged.last_mut().filter(|last| last.end == r.start) {
                last.end = r.end;
            } else {
                merged.push(r);
            }
        }
        merged
    }

    fn visit(&self, at: u16, view: &Bounds2, clip: &std::ops::Range<u32>, out: &mut Vec<std::ops::Range<u32>>) {
        // Trees are built from at most 64 leaves; recursion is bounded to six edges.
        let Some(node) = self.nodes.get(usize::from(at)) else { return };
        if !node.bounds().intersects(view) {
            return;
        }
        if node.count != 0 {
            let first = u32::from(node.first).max(clip.start);
            let end = (u32::from(node.first) + u32::from(node.count)).min(clip.end);
            if first < end {
                out.push(first..end);
            }
        } else {
            self.visit(node.left, view, clip, out);
            self.visit(node.right, view, clip, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn spatial_ranges_reject_distant_geometry_without_reordering_or_splitting_triangles() {
        let mut bytes = Vec::new();
        for i in 0..128 {
            let x = if (i / 8) % 2 == 0 { 10.0 } else { 10_000.0 };
            for p in [[x, 0.0], [x + 1.0, 0.0], [x, 1.0]] {
                push_vertex(&mut bytes, p, [0, 0, 0, 128]);
            }
        }
        let index = Index::new(&bytes, VERTEX_SIZE).unwrap();
        let view = Bounds2::new(Vec2::ZERO, Vec2::new(20.0, 20.0));
        let ranges = index.ranges(&view, 0..384);
        assert_eq!(ranges, (0..8).map(|i| i * 48..i * 48 + 24).collect::<Vec<_>>());
        assert_eq!(index.ranges(&view, 3..12), vec![3..12]);
        assert!(index.ranges(&Bounds2::new(Vec2::new(100.0, 100.0), Vec2::new(110.0, 110.0)), 0..384).is_empty());
        assert_eq!(index.ranges(&Bounds2::new(Vec2::new(-1.0, -1.0), Vec2::new(20_000.0, 20.0)), 0..384), vec![0..384]);
    }
}
