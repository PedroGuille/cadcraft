//! Bounded per-view/tile operation lists. Uploads invalidate every plan; order is never sorted.
use super::*;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Default)]
pub(super) struct Plans {
    bounds: Vec<Bounds2>,
    index: Option<culling::placements::Index>,
    all: Arc<[usize]>,
    recent: Mutex<VecDeque<(Bounds2, Arc<[usize]>)>>,
    limit: AtomicUsize,
}

impl Plans {
    pub fn bytes(&self) -> usize {
        self.recent.try_lock().map_or(0, |recent| retained(&recent))
    }
    pub fn clear(&self) {
        if let Ok(mut recent) = self.recent.try_lock() {
            recent.clear();
        }
    }
    pub fn rebuild(&mut self, bounds: Vec<Bounds2>) {
        self.index = culling::placements::Index::new(&bounds);
        self.all = (0..bounds.len()).collect::<Vec<_>>().into();
        self.bounds = bounds;
        self.recent.get_mut().unwrap_or_else(std::sync::PoisonError::into_inner).clear();
    }

    pub fn set_budget(&self, bytes: usize) {
        self.limit.store(bytes.min(2 << 20), Ordering::Relaxed);
        if let Ok(mut recent) = self.recent.try_lock() {
            while retained(&recent) > bytes {
                if recent.pop_front().is_none() {
                    break;
                }
            }
        }
    }

    pub fn query(&self, view: Option<Bounds2>) -> Arc<[usize]> {
        let Some(view) = view.filter(|v| !v.is_empty() && v.min.is_finite() && v.max.is_finite()) else { return self.all.clone() };
        if let Ok(mut recent) = self.recent.try_lock()
            && let Some(at) = recent.iter().position(|(area, _)| *area == view)
            && let Some(entry) = recent.remove(at)
        {
            let answer = entry.1.clone();
            recent.push_back(entry);
            return answer;
        }
        let ids: Arc<[usize]> = if let Some(index) = &self.index {
            index.indices(&self.bounds, &view).into()
        } else {
            self.bounds
                .iter()
                .enumerate()
                .filter(|(_, b)| b.is_empty() || !b.min.is_finite() || !b.max.is_finite() || b.intersects(&view))
                .map(|(i, _)| i)
                .collect::<Vec<_>>()
                .into()
        };
        let bytes = std::mem::size_of_val(ids.as_ref()) + std::mem::size_of::<(Bounds2, Arc<[usize]>)>();
        let limit = self.limit.load(Ordering::Relaxed);
        if bytes <= limit
            && let Ok(mut recent) = self.recent.try_lock()
        {
            while !recent.is_empty() && (recent.len() >= 64 || retained(&recent).saturating_add(bytes) > limit) {
                recent.pop_front();
            }
            recent.push_back((view, ids.clone()));
        }
        ids
    }
}

fn retained(recent: &VecDeque<(Bounds2, Arc<[usize]>)>) -> usize {
    recent.iter().map(|(_, ids)| std::mem::size_of_val(ids.as_ref()) + std::mem::size_of::<(Bounds2, Arc<[usize]>)>()).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tile_plans_preserve_order_reuse_and_invalidate_after_edits_and_pressure() {
        let mut plans = Plans::default();
        plans.set_budget(4096);
        let far = Bounds2::new(Vec2::new(100.0, 100.0), Vec2::new(200.0, 200.0));
        let near = Bounds2::new(Vec2::ZERO, Vec2::new(1.0, 1.0));
        plans.rebuild(vec![near, far, Bounds2::EMPTY, near]);
        let first = plans.query(Some(near));
        assert_eq!(first.as_ref(), &[0, 2, 3]);
        assert!(Arc::ptr_eq(&first, &plans.query(Some(near))));
        plans.set_budget(0);
        assert!(!Arc::ptr_eq(&first, &plans.query(Some(near))));
        plans.rebuild(vec![far, near]);
        assert_eq!(plans.query(Some(near)).as_ref(), &[1]);
        assert_eq!(plans.query(None).as_ref(), &[0, 1]);
    }
}
