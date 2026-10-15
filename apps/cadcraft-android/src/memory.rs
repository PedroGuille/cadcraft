//! Reconstructible caches share headroom with the entire native drawing session.
#![forbid(unsafe_code)]

pub struct State {
    pub total: u64,
    pub available: u64,
    pub low: bool,
}

/// Native allocations, open drawings and undo all contribute to RSS, unlike Java heap limits.
pub fn resident_bytes() -> Option<u64> {
    std::fs::read_to_string("/proc/self/status").ok()?.lines().find_map(|line| {
        let value = line.strip_prefix("VmRSS:")?.split_whitespace().next()?.parse::<u64>().ok()?;
        value.checked_mul(1024)
    })
}

/// This is a cache allowance, not a hard process limit. Never discard drawing or undo data.
pub fn cache_mib(requested: u32, resident: Option<u64>, state: State) -> u32 {
    if state.low || state.total == 0 {
        return 0;
    }
    let reserve = (state.total / 16).max(64 << 20);
    let session = (state.total / 8).clamp(128 << 20, 1024 << 20);
    let headroom = state.available.saturating_sub(reserve).min(session.saturating_sub(resident.unwrap_or(0)));
    u64::from(requested.min(64)).min((headroom / 4) >> 20) as u32
}
