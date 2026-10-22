//! Scoped worker cancellation. A cancelled build is discarded, never installed as a mesh.
use std::cell::RefCell;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

thread_local! { static CURRENT: RefCell<Option<Arc<AtomicBool>>> = const { RefCell::new(None) }; }

pub(super) fn cancelled() -> bool {
    CURRENT
        .try_with(|slot| slot.try_borrow().ok().is_some_and(|flag| flag.as_ref().is_some_and(|flag| flag.load(Ordering::Relaxed))))
        .unwrap_or(false)
}

struct Restore(Option<Arc<AtomicBool>>);
impl Drop for Restore {
    fn drop(&mut self) {
        let _ = CURRENT.try_with(|slot| {
            if let Ok(mut slot) = slot.try_borrow_mut() {
                *slot = self.0.take();
            }
        });
    }
}

pub(super) fn scope<T>(flag: Option<Arc<AtomicBool>>, run: impl FnOnce() -> T) -> Option<T> {
    let old = CURRENT.try_with(|slot| slot.try_borrow_mut().ok().map(|mut slot| std::mem::replace(&mut *slot, flag))).ok().flatten();
    let _restore = Restore(old.flatten());
    if cancelled() {
        return None;
    }
    let result = run();
    (!cancelled()).then_some(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nested_cancelled_work_is_discarded_and_scope_restores_the_caller() {
        let flag = Arc::new(AtomicBool::new(false));
        let answer = scope(Some(flag.clone()), || {
            assert!(!cancelled());
            flag.store(true, Ordering::Relaxed);
            assert!(cancelled());
            assert_eq!(scope(None, cancelled), Some(false));
            assert!(cancelled());
            42
        });
        assert_eq!(answer, None);
        assert!(!cancelled());
    }
}
