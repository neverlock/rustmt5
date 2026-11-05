//! Async token bucket rate limiter (port of `pymt5._rate_limiter`).

use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// `rate` tokens/second, bursting up to `burst`. `rate <= 0` disables limiting.
#[derive(Debug)]
pub struct TokenBucketRateLimiter {
    pub rate: f64,
    pub burst: u32,
    state: Mutex<(f64, Instant)>,
}

impl TokenBucketRateLimiter {
    pub fn new(rate: f64, burst: u32) -> Self {
        Self { rate, burst, state: Mutex::new((burst as f64, Instant::now())) }
    }

    /// Wait until a token is available, then consume it.
    pub async fn acquire(&self) {
        if self.rate <= 0.0 {
            return;
        }
        loop {
            let wait = {
                let mut st = self.state.lock().await;
                let now = Instant::now();
                let elapsed = now.duration_since(st.1).as_secs_f64();
                st.0 = (st.0 + elapsed * self.rate).min(self.burst as f64);
                st.1 = now;
                if st.0 >= 1.0 {
                    st.0 -= 1.0;
                    return;
                }
                (1.0 - st.0) / self.rate
            };
            tokio::time::sleep(Duration::from_secs_f64(wait)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn disabled_passes_immediately() {
        let l = TokenBucketRateLimiter::new(0.0, 1);
        let t = Instant::now();
        for _ in 0..100 { l.acquire().await; }
        assert!(t.elapsed() < Duration::from_millis(50));
    }

    #[tokio::test]
    async fn burst_then_throttle() {
        let l = TokenBucketRateLimiter::new(20.0, 2);
        let t = Instant::now();
        for _ in 0..4 { l.acquire().await; }
        // 2 burst tokens free, 2 more need ~100ms at 20/s
        assert!(t.elapsed() >= Duration::from_millis(80), "{:?}", t.elapsed());
    }
}
