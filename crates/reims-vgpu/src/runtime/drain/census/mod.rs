//! Functional route counters.
//!
//! Many correctness tests assert which arm of a functional decision ran by
//! counting the route it names. The counters exist only under `cfg(test)`;
//! production builds compile every call down to nothing.

pub mod stall;

#[cfg(test)]
static STORE_ROUTES: std::sync::LazyLock<
    std::sync::Mutex<std::collections::BTreeMap<&'static str, u64>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::BTreeMap::new()));

#[inline(always)]
pub fn note_store_route(route: &'static str) {
    #[cfg(test)]
    {
        let mut routes = STORE_ROUTES.lock().unwrap_or_else(|e| e.into_inner());
        *routes.entry(route).or_default() += 1;
    }
    #[cfg(not(test))]
    let _ = route;
}

#[inline(always)]
pub fn note_store_route_n(route: &'static str, n: u64) {
    #[cfg(test)]
    {
        let mut routes = STORE_ROUTES.lock().unwrap_or_else(|e| e.into_inner());
        *routes.entry(route).or_default() += n;
    }
    #[cfg(not(test))]
    let _ = (route, n);
}

#[cfg(test)]
pub(crate) fn store_route_count(route: &str) -> u64 {
    STORE_ROUTES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(route)
        .copied()
        .unwrap_or(0)
}
