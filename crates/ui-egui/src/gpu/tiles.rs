//! Damage tracking for a tiled image atlas. Geometry is replayed in its complete CAD order
//! inside dirty scissors; unchanged pixels are retained, including through integral pans.

use cadcraft_geom::Bounds2;
use cadcraft_render::screen::DamagePart;
use egui::{Rect, pos2, vec2};
use std::collections::{HashMap, HashSet};

pub(super) const EDGE: u32 = 512;

/// Unchanged chunks are cheap ordering anchors. Only replaced chunks need entity comparisons;
/// an edit to a scattered chunk must not dirty all the other objects it contains.
pub(super) fn damage(old: &[DamagePart], new: &[DamagePart]) -> Option<Vec<Bounds2>> {
    if old.len().max(new.len()) > 8192 || (old.is_empty() && new.is_empty()) {
        return None;
    }
    #[derive(Clone, Copy, PartialEq, Eq, Hash)]
    enum Token {
        Part(u64),
        Entity(u64, usize),
    }
    let before: HashSet<_> = old.iter().map(|p| p.id).collect();
    let after: HashSet<_> = new.iter().map(|p| p.id).collect();
    let expand = |parts: &[DamagePart], other: &HashSet<u64>| {
        let mut result = Vec::new();
        for p in parts {
            if other.contains(&p.id) || p.details.is_none() {
                result.push((Token::Part(p.id), Bounds2 { min: p.bounds.min - p.origin, max: p.bounds.max - p.origin }));
            } else if let Some(d) = &p.details {
                if result.len().saturating_add(d.entities.len()) > 32_768 {
                    return None;
                }
                result.extend(d.entities.iter().map(|e| {
                    (
                        Token::Entity(d.context, std::sync::Arc::as_ptr(&e.source) as usize),
                        if e.bounds == Bounds2::EMPTY { e.bounds } else { Bounds2 { min: e.bounds.min - p.origin, max: e.bounds.max - p.origin } },
                    )
                }));
            }
        }
        Some(result)
    };
    let (a, b) = (expand(old, &after)?, expand(new, &before)?);
    let before: HashMap<_, _> = a.iter().copied().collect();
    let after: HashMap<_, _> = b.iter().copied().collect();
    if before.len() != a.len()
        || after.len() != b.len()
        || !a.iter().map(|(id, _)| id).filter(|id| after.contains_key(id)).eq(b.iter().map(|(id, _)| id).filter(|id| before.contains_key(id)))
    {
        return None;
    }
    let mut dirty = Vec::new();
    for (entries, other) in [(&a, &after), (&b, &before)] {
        for (id, bounds) in entries {
            if other.get(id) != Some(bounds) {
                if *bounds == Bounds2::EMPTY {
                    continue;
                }
                if bounds.is_empty() || !bounds.min.is_finite() || !bounds.max.is_finite() {
                    return None;
                }
                dirty.push(*bounds);
            }
        }
    }
    Some(dirty)
}

/// Pixel scissors for the dirty tiles. `valid` is the part of the clip copied from the old
/// image after a pan; exposed tile edges are cleared and replayed too.
pub(super) fn dirty(size: [u32; 2], clip: Rect, damage: &[Rect], valid: Rect) -> Vec<[u32; 4]> {
    let mut result = Vec::new();
    for y in (0..size[1]).step_by(EDGE as usize) {
        for x in (0..size[0]).step_by(EDGE as usize) {
            let tile =
                Rect::from_min_size(pos2(x as f32, y as f32), vec2(EDGE.min(size[0] - x) as f32, EDGE.min(size[1] - y) as f32)).intersect(clip);
            if tile.is_positive() && (!valid.contains_rect(tile) || damage.iter().any(|d| d.intersects(tile))) {
                let left = tile.min.x.floor().max(0.0) as u32;
                let top = tile.min.y.floor().max(0.0) as u32;
                let right = (tile.max.x.ceil().max(0.0) as u32).min(size[0]);
                let bottom = (tile.max.y.ceil().max(0.0) as u32).min(size[1]);
                if right > left && bottom > top {
                    result.push([left, top, right - left, bottom - top]);
                }
            }
        }
    }
    result
}

pub(super) struct Copy {
    pub from: [u32; 2],
    pub to: [u32; 2],
    pub size: [u32; 2],
}

/// A single bounded scratch tile is enough to scroll an atlas. Copy from the far edge back
/// toward the exposed edge, preventing a destination from overwriting an unread source.
pub(super) fn copies(size: [u32; 2], shift: [i32; 2]) -> Vec<Copy> {
    let [dx, dy] = shift.map(i64::from);
    let [w, h] = size.map(i64::from);
    let (left, top, right, bottom) = ((-dx).max(0), (-dy).max(0), (w - dx).min(w), (h - dy).min(h));
    if right <= left || bottom <= top {
        return Vec::new();
    }
    let mut xs: Vec<_> = (left..right).step_by(EDGE as usize).collect();
    let mut ys: Vec<_> = (top..bottom).step_by(EDGE as usize).collect();
    if dx > 0 {
        xs.reverse();
    }
    if dy > 0 {
        ys.reverse();
    }
    let mut result = Vec::new();
    for y in ys {
        for &x in &xs {
            result.push(Copy {
                from: [x as u32, y as u32],
                to: [(x + dx) as u32, (y + dy) as u32],
                size: [(right - x).min(i64::from(EDGE)) as u32, (bottom - y).min(i64::from(EDGE)) as u32],
            });
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use cadcraft_geom::Vec2;

    fn parts(items: &[(u64, Bounds2)]) -> Vec<DamagePart> {
        items.iter().map(|(id, bounds)| DamagePart { id: *id, bounds: *bounds, details: None, origin: cadcraft_geom::Vec2::ZERO }).collect()
    }

    #[test]
    fn moving_deleting_and_reordering_parts_invalidate_their_old_pixels() {
        let a = Bounds2::new(Vec2::ZERO, Vec2::new(20.0, 20.0));
        let b = Bounds2::new(Vec2::new(100.0, 100.0), Vec2::new(120.0, 120.0));
        assert!(damage(&parts(&[(1, a), (2, b)]), &parts(&[(1, a), (3, a)])).unwrap().contains(&b));
        assert_eq!(damage(&parts(&[(1, a)]), &parts(&[(1, a)])).unwrap(), Vec::<Bounds2>::new());
        assert!(damage(&parts(&[(1, a), (2, b)]), &parts(&[(2, b), (1, a)])).is_none());
        let clip = Rect::from_min_size(pos2(0.0, 0.0), vec2(1536.0, 1024.0));
        let small = Rect::from_min_size(pos2(100.0, 100.0), vec2(20.0, 20.0));
        assert_eq!(dirty([1536, 1024], clip, &[small], clip).len(), 1);
        assert!(dirty([1536, 1024], clip, &[], clip).is_empty());
    }

    #[test]
    fn scroll_copy_order_preserves_every_unexposed_pixel_in_all_directions() {
        let size = [1031, 773];
        let original: Vec<_> = (0..size[0] * size[1]).collect();
        for shift in [[1, 0], [-1, 0], [0, 1], [0, -1], [7, 11], [-7, 11], [7, -11], [-7, -11], [519, 517], [-519, -517]] {
            let mut moved = original.clone();
            for region in copies(size, shift) {
                let mut scratch = Vec::new();
                for y in 0..region.size[1] {
                    let start = ((region.from[1] + y) * size[0] + region.from[0]) as usize;
                    scratch.extend_from_slice(&moved[start..start + region.size[0] as usize]);
                }
                for y in 0..region.size[1] {
                    let start = ((region.to[1] + y) * size[0] + region.to[0]) as usize;
                    let offset = (y * region.size[0]) as usize;
                    moved[start..start + region.size[0] as usize].copy_from_slice(&scratch[offset..offset + region.size[0] as usize]);
                }
            }
            for y in 0..size[1] {
                for x in 0..size[0] {
                    let source = [i64::from(x) - i64::from(shift[0]), i64::from(y) - i64::from(shift[1])];
                    if source[0] >= 0 && source[0] < i64::from(size[0]) && source[1] >= 0 && source[1] < i64::from(size[1]) {
                        assert_eq!(
                            moved[(y * size[0] + x) as usize],
                            original[(source[1] * i64::from(size[0]) + source[0]) as usize],
                            "{shift:?} at {x}, {y}"
                        );
                    }
                }
            }
        }
    }
}
