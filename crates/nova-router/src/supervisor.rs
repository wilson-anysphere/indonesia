use std::time::Duration;

#[derive(Debug, Clone)]
pub struct RestartBackoff {
    initial: Duration,
    max: Duration,
    next: Duration,
}

impl RestartBackoff {
    pub fn new(initial: Duration, max: Duration) -> Self {
        Self {
            initial,
            max,
            next: initial,
        }
    }

    pub fn reset(&mut self) {
        self.next = self.initial;
    }

    pub fn next_delay(&mut self) -> Duration {
        let delay = self.next;
        let doubled = self.next.checked_mul(2).unwrap_or(self.max);
        self.next = doubled.min(self.max);
        delay
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restart_backoff_exponentially_increases_until_capped() {
        let mut backoff = RestartBackoff::new(Duration::from_millis(50), Duration::from_secs(5));
        assert_eq!(backoff.next_delay(), Duration::from_millis(50));
        assert_eq!(backoff.next_delay(), Duration::from_millis(100));
        assert_eq!(backoff.next_delay(), Duration::from_millis(200));
        assert_eq!(backoff.next_delay(), Duration::from_millis(400));
        assert_eq!(backoff.next_delay(), Duration::from_millis(800));
        assert_eq!(backoff.next_delay(), Duration::from_millis(1600));
        assert_eq!(backoff.next_delay(), Duration::from_millis(3200));
        assert_eq!(backoff.next_delay(), Duration::from_secs(5));
        assert_eq!(backoff.next_delay(), Duration::from_secs(5));
    }

    #[test]
    fn restart_backoff_can_be_reset_after_success() {
        let mut backoff = RestartBackoff::new(Duration::from_millis(50), Duration::from_secs(5));
        let _ = backoff.next_delay();
        let _ = backoff.next_delay();
        backoff.reset();
        assert_eq!(backoff.next_delay(), Duration::from_millis(50));
    }
}
