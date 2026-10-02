//! Who may draw on the HUD, and the rules that keep a late or stale request from drawing over a
//! newer one. The core is the only owner of the recording HUD; each presentation it sends carries
//! the epoch it was issued under, and the overlay drops anything older than the newest `Show` it
//! has adopted. Platform-neutral so the rules are testable without a window.

use crate::event::{JobId, RequestId, SessionId};
use crate::win::overlay::Tone;
use std::time::{Duration, Instant};

/// What a HUD presentation belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HudOwner {
    Dictation(SessionId),
    Retry(JobId),
    Notice(RequestId),
}

#[derive(Clone, Debug, PartialEq)]
pub enum HudCommand {
    Show {
        text: String,
        tone: Tone,
        hide_after: Option<Duration>,
        /// Clears the transcript panel (a status change keeps it).
        clear: bool,
    },
    Words(String, String),
    /// Recording stops this long from now.
    Limit(Duration),
    Done,
    Hide,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OwnedPresentation {
    pub epoch: u64,
    pub owner: HudOwner,
    pub command: HudCommand,
}

/// Admits commands by epoch. `None` is an unowned caller (startup errors, the debug demo) and
/// is always applied without disturbing the current epoch.
#[derive(Debug, Default)]
pub struct EpochGate {
    current: u64,
}

impl EpochGate {
    pub fn current(&self) -> u64 {
        self.current
    }

    /// A `Show` with a newer (or equal) epoch takes the HUD; older ones are dropped. Every
    /// other command applies only to the epoch that is on screen.
    pub fn admit(&mut self, epoch: Option<u64>, show: bool) -> bool {
        let Some(e) = epoch else { return true };
        if show {
            if e < self.current {
                return false;
            }
            self.current = e;
            true
        } else {
            e == self.current
        }
    }
}

/// A queued WM_TIMER can outlive `KillTimer`, so a hide fires only if it was armed for the
/// epoch now on screen and its deadline has really passed.
#[derive(Debug, Default)]
pub struct HideTimer {
    armed: Option<(u64, Instant)>,
}

impl HideTimer {
    const EARLY_TOLERANCE: Duration = Duration::from_millis(20);

    pub fn arm(&mut self, epoch: u64, now: Instant, after: Option<Duration>) {
        self.armed = after.map(|d| (epoch, now + d));
    }

    pub fn fire(&mut self, epoch: u64, now: Instant) -> bool {
        match self.armed {
            Some((e, at)) if e == epoch && now + Self::EARLY_TOLERANCE >= at => {
                self.armed = None;
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_show_wins_and_older_is_dropped() {
        let mut g = EpochGate::default();
        assert!(g.admit(Some(5), true));
        assert!(!g.admit(Some(4), true), "an older show is stale");
        assert!(g.admit(Some(5), true), "same owner may change its status");
        assert!(g.admit(Some(6), true));
        assert_eq!(g.current(), 6);
    }

    #[test]
    fn non_show_commands_need_the_current_epoch() {
        let mut g = EpochGate::default();
        g.admit(Some(7), true);
        assert!(g.admit(Some(7), false));
        assert!(
            !g.admit(Some(6), false),
            "late words from a finished dictation"
        );
        assert!(!g.admit(Some(8), false), "no show adopted it yet");
    }

    #[test]
    fn unowned_commands_always_apply_and_leave_the_epoch_alone() {
        let mut g = EpochGate::default();
        g.admit(Some(3), true);
        assert!(g.admit(None, true));
        assert!(g.admit(None, false));
        assert_eq!(g.current(), 3);
    }

    #[test]
    fn stale_hide_timer_for_an_old_epoch_does_not_hide_the_new_one() {
        let t0 = Instant::now();
        let mut h = HideTimer::default();
        h.arm(1, t0, Some(Duration::from_secs(2)));
        // The next owner shows with no auto-hide; the old timer message was already queued.
        h.arm(2, t0 + Duration::from_secs(1), None);
        assert!(!h.fire(2, t0 + Duration::from_secs(2)));
        assert!(!h.fire(1, t0 + Duration::from_secs(2)));
    }

    #[test]
    fn hide_fires_once_for_its_own_epoch_after_its_deadline() {
        let t0 = Instant::now();
        let mut h = HideTimer::default();
        h.arm(4, t0, Some(Duration::from_secs(2)));
        assert!(
            !h.fire(4, t0 + Duration::from_secs(1)),
            "queued early message"
        );
        assert!(!h.fire(5, t0 + Duration::from_secs(3)), "different epoch");
        assert!(h.fire(4, t0 + Duration::from_secs(2)));
        assert!(!h.fire(4, t0 + Duration::from_secs(3)), "only once");
    }

    #[test]
    fn a_rearmed_timer_replaces_the_previous_deadline() {
        let t0 = Instant::now();
        let mut h = HideTimer::default();
        h.arm(1, t0, Some(Duration::from_secs(1)));
        h.arm(1, t0 + Duration::from_secs(1), Some(Duration::from_secs(4)));
        assert!(!h.fire(1, t0 + Duration::from_secs(2)));
        assert!(h.fire(1, t0 + Duration::from_secs(5)));
    }
}
