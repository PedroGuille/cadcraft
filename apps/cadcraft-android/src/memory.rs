//! Reconstructible caches grow on demand and yield to live drawings and system headroom.
#![forbid(unsafe_code)]

pub struct State {
    pub total: u64,
    pub available: u64,
    pub low: bool,
}

#[cfg(target_os = "android")]
pub fn resident_bytes() -> Option<u64> {
    std::fs::read_to_string("/proc/self/status").ok()?.lines().find_map(|line| {
        let value = line.strip_prefix("VmRSS:")?.split_whitespace().next()?.parse::<u64>().ok()?;
        value.checked_mul(1024)
    })
}

/// A ceiling for optional caches, never a reservation or a limit on live drawing data.
/// Hysteresis avoids shrinking caches merely because their own allocations consumed headroom.
pub fn cache_mib(requested: u32, current: Option<u32>, resident: Option<u64>, state: State) -> u32 {
    if state.low || state.total == 0 {
        return 0;
    }
    let requested = requested.min(2048);
    let reserve = (state.total / 4).max(256 << 20);
    let session = (state.total / 2).clamp(128 << 20, 4u64 << 30);
    let resident = resident.unwrap_or(session);
    let Some(current) = current else {
        return u64::from(requested).min(state.available.saturating_sub(reserve).min(session.saturating_sub(resident)) >> 20) as u32;
    };
    let current = current.min(requested);
    let deficit = reserve.saturating_sub(state.available).max(resident.saturating_sub(session));
    if deficit > 0 {
        return current.saturating_sub(((deficit >> 20).min(u64::from(u32::MAX)) as u32).saturating_add(128));
    }
    if state.available > reserve.saturating_add(256 << 20) && resident < session.saturating_sub(256 << 20) {
        current.saturating_add(128).min(requested)
    } else {
        current
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retains_a_large_cache_without_chasing_its_own_allocations() {
        let state = State { total: 8 << 30, available: 4 << 30, low: false };
        assert_eq!(cache_mib(2048, None, Some(512 << 20), state), 2048);
        let state = State { total: 8 << 30, available: 3 << 30, low: false };
        assert_eq!(cache_mib(2048, None, Some(512 << 20), state), 1024);
        let state = State { total: 8 << 30, available: (2 << 30) + (128 << 20), low: false };
        assert_eq!(cache_mib(2048, Some(2048), Some(3 << 30), state), 2048);
    }
    #[test]
    fn pressure_trims_and_recovery_grows_gradually() {
        let state = State { total: 6 << 30, available: 1 << 30, low: false };
        assert!(cache_mib(2048, Some(2048), Some(2 << 30), state) < 2048);
        let state = State { total: 8 << 30, available: 5 << 30, low: false };
        assert_eq!(cache_mib(2048, Some(0), Some(512 << 20), state), 128);
        assert_eq!(cache_mib(2048, Some(2048), Some(512 << 20), State { total: 8 << 30, available: 5 << 30, low: true }), 0);
    }
}
