use std::time::{Duration, Instant};

pub const STARTUP_TIMEOUT: Duration = Duration::from_secs(120);
const PAUSE_THRESHOLD: Duration = Duration::from_secs(10);

#[derive(Debug, PartialEq)]
pub enum WatchdogEvent {
    StartupTimeout,
    HeartbeatTimeout,
    Resumed,
}

pub struct WatchdogState {
    pub last_heartbeat: Option<Instant>,
    pub started_at: Option<Instant>,
    last_check: Option<Instant>,
    grace_started_at: Option<Instant>,
    unresponsive: bool,
}

impl WatchdogState {
    pub fn new() -> Self {
        Self {
            last_heartbeat: None,
            started_at: None,
            last_check: None,
            grace_started_at: None,
            unresponsive: false,
        }
    }

    pub fn start(&mut self, now: Instant) -> bool {
        if self.started_at.is_some() {
            return false;
        }
        self.started_at = Some(now);
        self.last_check = Some(now);
        self.grace_started_at = Some(now);
        true
    }

    pub fn heartbeat(&mut self, now: Instant) -> bool {
        self.last_heartbeat = Some(now);
        let recovered = self.unresponsive;
        self.unresponsive = false;
        recovered
    }

    pub fn check(&mut self, now: Instant, timeout: Duration) -> Option<WatchdogEvent> {
        let last_check = self.last_check.replace(now)?;
        // A suspended process cannot observe heartbeats. Allow a full interval after resuming.
        if now.duration_since(last_check) > PAUSE_THRESHOLD {
            self.grace_started_at = Some(now);
            return Some(WatchdogEvent::Resumed);
        }
        if self.unresponsive {
            return None;
        }
        let grace = self.grace_started_at.unwrap();
        let (last_response, threshold, event) = match self.last_heartbeat {
            Some(last) => (last.max(grace), timeout, WatchdogEvent::HeartbeatTimeout),
            None => (grace, STARTUP_TIMEOUT, WatchdogEvent::StartupTimeout),
        };
        if now.duration_since(last_response) < threshold {
            return None;
        }
        self.unresponsive = true;
        Some(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TIMEOUT: Duration = Duration::from_secs(30);

    fn tick(
        state: &mut WatchdogState,
        start: Instant,
        from: u64,
        to: u64,
    ) -> Option<WatchdogEvent> {
        let mut event = None;
        for seconds in from..=to {
            event = state.check(start + Duration::from_secs(seconds), TIMEOUT);
        }
        event
    }

    #[test]
    fn startup_gets_two_minutes_and_start_is_idempotent() {
        let now = Instant::now();
        let mut state = WatchdogState::new();
        assert!(state.start(now));
        assert!(!state.start(now + Duration::from_secs(60)));
        assert_eq!(tick(&mut state, now, 1, 119), None);
        assert_eq!(
            tick(&mut state, now, 120, 120),
            Some(WatchdogEvent::StartupTimeout)
        );
        assert_eq!(tick(&mut state, now, 121, 150), None);
        assert!(state.heartbeat(now + Duration::from_secs(150)));
        assert_eq!(
            tick(&mut state, now, 151, 180),
            Some(WatchdogEvent::HeartbeatTimeout)
        );
    }

    #[test]
    fn timeout_notifies_once_and_heartbeat_rearms_it() {
        let now = Instant::now();
        let mut state = WatchdogState::new();
        state.start(now);
        assert!(!state.heartbeat(now));
        assert_eq!(tick(&mut state, now, 1, 29), None);
        assert_eq!(
            tick(&mut state, now, 30, 30),
            Some(WatchdogEvent::HeartbeatTimeout)
        );
        assert_eq!(tick(&mut state, now, 31, 60), None);
        assert!(state.heartbeat(now + Duration::from_secs(60)));
        assert!(!state.heartbeat(now + Duration::from_secs(60)));
        assert_eq!(
            tick(&mut state, now, 61, 90),
            Some(WatchdogEvent::HeartbeatTimeout)
        );
    }

    #[test]
    fn process_pause_grants_full_timeout_without_fabricating_heartbeat() {
        let now = Instant::now();
        let mut state = WatchdogState::new();
        state.start(now);
        state.heartbeat(now);
        assert_eq!(
            state.check(now + Duration::from_secs(600), TIMEOUT),
            Some(WatchdogEvent::Resumed)
        );
        assert_eq!(state.last_heartbeat, Some(now));
        assert_eq!(tick(&mut state, now, 601, 629), None);
        assert_eq!(
            tick(&mut state, now, 630, 630),
            Some(WatchdogEvent::HeartbeatTimeout)
        );
        assert_eq!(
            state.check(now + Duration::from_secs(1200), TIMEOUT),
            Some(WatchdogEvent::Resumed)
        );
        assert_eq!(tick(&mut state, now, 1201, 1230), None);
    }

    #[test]
    fn startup_pause_grants_full_startup_window() {
        let now = Instant::now();
        let mut state = WatchdogState::new();
        state.start(now);
        assert_eq!(
            state.check(now + Duration::from_secs(600), TIMEOUT),
            Some(WatchdogEvent::Resumed)
        );
        assert_eq!(tick(&mut state, now, 601, 719), None);
        assert_eq!(
            tick(&mut state, now, 720, 720),
            Some(WatchdogEvent::StartupTimeout)
        );
    }
}
