//! The one HTTP client: Web API, art, lyrics and sign-in all share it, so connections to a host
//! are reused instead of being opened per module.

use std::sync::OnceLock;
use std::time::Duration;

pub fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .connect_timeout(Duration::from_secs(8))
            .pool_max_idle_per_host(6)
            // Idle connections are dropped after a while so nothing lingers during long sessions.
            .pool_idle_timeout(Duration::from_secs(60))
            .build()
            .unwrap_or_default()
    })
}

/// Tries after the first for requests that are safe to repeat.
pub const RETRIES: u32 = 3;

/// Capped exponential backoff: 0.5 s, 1 s, 2 s, 4 s, then 8 s.
pub fn backoff(attempt: u32) -> Duration {
    Duration::from_millis(500u64.saturating_mul(1 << attempt.min(4)).min(8000))
}

/// Longest `Retry-After` that is waited out. Spotify answers abuse with waits of hours; sitting
/// on a request that long would look like a hang, so those are reported instead.
pub const MAX_RETRY_AFTER: u64 = 60;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_caps() {
        let ms: Vec<u128> = (0..7).map(|a| backoff(a).as_millis()).collect();
        assert_eq!(ms, [500, 1000, 2000, 4000, 8000, 8000, 8000]);
    }
}
