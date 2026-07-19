use std::collections::VecDeque;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// At most `max_calls` calls within any rolling `window`. Callers await
/// `acquire()` immediately before the call it's guarding; it returns at
/// once if there's room, or sleeps until a slot frees up. Deliberately
/// hand-rolled on std+tokio rather than pulling in a crate like
/// `governor` - after the edition2024 dependency fights earlier in this
/// project, a well-understood 20-line implementation is worth more than
/// a black-box crate here.
pub struct RateLimiter {
    max_calls: usize,
    window: Duration,
    timestamps: Mutex<VecDeque<Instant>>,
}

impl RateLimiter {
    pub fn new(max_calls: usize, window: Duration) -> Self {
        Self {
            max_calls,
            window,
            timestamps: Mutex::new(VecDeque::with_capacity(max_calls)),
        }
    }

    pub async fn acquire(&self) {
        loop {
            let wait = {
                let mut ts = self.timestamps.lock().await;
                let now = Instant::now();
                while let Some(&front) = ts.front() {
                    if now.duration_since(front) > self.window {
                        ts.pop_front();
                    } else {
                        break;
                    }
                }
                if ts.len() < self.max_calls {
                    ts.push_back(now);
                    None
                } else {
                    let oldest = *ts.front().unwrap();
                    Some(self.window.saturating_sub(now.duration_since(oldest)))
                }
            };
            match wait {
                None => return,
                Some(d) => tokio::time::sleep(d).await,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn allows_burst_up_to_the_limit_then_throttles() {
        let limiter = Arc::new(RateLimiter::new(3, Duration::from_millis(200)));
        let start = Instant::now();
        for _ in 0..3 {
            limiter.acquire().await; // should be immediate
        }
        assert!(start.elapsed() < Duration::from_millis(50));
        limiter.acquire().await; // 4th call should have to wait out the window
        assert!(start.elapsed() >= Duration::from_millis(150));
    }
}
