use std::sync::Mutex;
use std::time::{Duration, Instant};

/// A release-aware local PTT lease. A stalled UI stops media within 750 ms;
/// a ten-second burst requires a new release/press. It adds no LXST wire fields.
#[derive(Default, Debug)]
pub struct AudioTransmitGate {
    state: Mutex<State>,
}
#[derive(Default, Debug)]
struct State {
    serial: u64,
    began: Option<Instant>,
    until: Option<Instant>,
    blocked: bool,
}
impl AudioTransmitGate {
    pub fn new() -> Self {
        Self::default()
    }
    /// Monotonically increasing serial for each edge; renew using the press
    /// serial. A delayed press/renew after a later release is rejected.
    pub fn update(&self, serial: u64, pressed: bool) -> bool {
        self.update_at(serial, pressed, Instant::now())
    }
    fn update_at(&self, serial: u64, pressed: bool, now: Instant) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        if serial == 0 || serial < state.serial {
            return false;
        }
        if serial > state.serial {
            state.serial = serial;
            state.began = None;
            state.until = None;
            if !pressed {
                state.blocked = false;
                return true;
            }
            if state.blocked {
                return false;
            }
            state.began = Some(now);
        }
        if !pressed {
            state.began = None;
            state.until = None;
            state.blocked = false;
            return true;
        }
        if state.blocked || state.began.is_none() {
            return false;
        }
        if now.duration_since(state.began.unwrap()) >= Duration::from_secs(10)
            || state.until.is_some_and(|end| now >= end)
        {
            state.blocked = true;
            state.until = None;
            return false;
        }
        state.until = Some(now + Duration::from_millis(750));
        true
    }
    pub fn serial(&self) -> u64 {
        self.state.lock().map(|s| s.serial).unwrap_or(0)
    }
    /// Permanently revoke this owner; replace it for a new session.
    pub fn close(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.serial = u64::MAX;
            state.until = None;
            state.began = None;
            state.blocked = true;
        }
    }
    pub fn allows(&self) -> bool {
        self.allows_at(Instant::now())
    }
    fn allows_at(&self, now: Instant) -> bool {
        let Ok(mut state) = self.state.try_lock() else {
            return false;
        };
        let allowed = !state.blocked
            && state.until.is_some_and(|end| now < end)
            && state
                .began
                .is_some_and(|begin| now.duration_since(begin) < Duration::from_secs(10));
        if !allowed && state.began.is_some() {
            state.blocked = true;
            state.until = None;
        }
        allowed
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn release_stale_renew_stall_and_burst_are_fenced() {
        let gate = AudioTransmitGate::new();
        let start = Instant::now();
        assert!(!gate.allows_at(start));
        assert!(gate.update_at(1, true, start));
        assert!(gate.allows_at(start));
        assert!(gate.update_at(2, false, start));
        assert!(!gate.update_at(1, true, start));
        assert!(gate.update_at(3, true, start));
        assert!(!gate.allows_at(start + Duration::from_secs(1)));
        assert!(!gate.update_at(4, true, start + Duration::from_secs(1))); // new press still requires release after timeout
        assert!(gate.update_at(5, false, start));
        assert!(gate.update_at(6, true, start));
        for ms in (250..10000).step_by(250) {
            assert!(gate.update_at(6, true, start + Duration::from_millis(ms)));
        }
        assert!(!gate.update_at(6, true, start + Duration::from_secs(10)));
        assert!(!gate.allows_at(start + Duration::from_secs(10)));
    }
}
