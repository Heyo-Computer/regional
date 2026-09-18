//! Shared dashboard state.

use crate::config::Config;
use crate::ratelimit::RateLimiter;
use crate::store::Store;

pub struct AppState {
    pub store: Store,
    pub config: Config,
    /// Guards the public form only; the admin port has no form to flood.
    pub limiter: RateLimiter,
}
