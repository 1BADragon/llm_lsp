//! Activity tracking: where am I working, and when have I *left* a symbol I
//! edited? A leave event is what triggers a passive review.
//!
//! ┌─────────────────────────────────────────────────────────────────────┐
//! │ STUB: yours to implement. Run the spec with                         │
//! │     cargo test activity -- --include-ignored                        │
//! │ Live: the server ticks the tracker once a second and logs each      │
//! │ LeaveEvent to Helix's log (`:log-open`, look for `leave:`).         │
//! └─────────────────────────────────────────────────────────────────────┘
//!
//! What the server can see (spikes/helix-log/FINDINGS.md): edits, plus hover /
//! completion requests, plus saves. It *cannot* see cursor movement, so
//! moving away without typing only shows up as silence. That's why the
//! timeout exists.
//!
//! All time is passed in (`at`, `now`), so tests never sleep.

use std::time::{Duration, Instant};

/// Identifies a symbol across edits. The qualified name rather than a body
/// hash, because the body changes while I type in it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SymbolId {
    pub uri: String,
    pub qualified_name: String,
}

#[derive(Debug, Clone)]
pub enum Signal {
    /// One `didChange` notification.
    Edit {
        uri: String,
        /// Every symbol the notification's changes landed in, deduplicated.
        /// Empty when the edit was outside any symbol (imports, top level).
        symbols: Vec<SymbolId>,
        /// Number of content changes in the notification (>1 for formatters
        /// and multi-cursor edits).
        ranges: usize,
        /// Distinct lines the changes touched.
        lines_touched: usize,
        at: Instant,
    },
    /// A hover or completion request (explicit attention at a position).
    /// `symbol` is `None` outside any symbol.
    Request {
        uri: String,
        symbol: Option<SymbolId>,
        at: Instant,
    },
    Save { uri: String, at: Instant },
}

#[derive(Debug, Clone)]
pub struct Config {
    /// How long an edited symbol must go without attention before it counts
    /// as left.
    pub leave_after: Duration,
    /// A bulk edit followed by a save of the same file within this window is
    /// a formatter, not me.
    pub formatter_window: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            leave_after: Duration::from_secs(10),
            formatter_window: Duration::from_millis(100),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaveEvent {
    pub symbol: SymbolId,
}

/// Rules (the tests below are the spec):
///
/// - Only symbols I **edited** can produce a leave event. Looking at or
///   hovering a symbol makes it focused, but doesn't make it dirty.
/// - The newest `Edit` / `Request` sets focus. That's its symbol, or nothing
///   if it was outside any symbol.
/// - A dirty symbol is **left** once it has had no attention (no Edit or
///   Request landing in it) for `leave_after`. It doesn't matter whether
///   focus moved elsewhere or everything just went quiet. Coming back to it
///   before then resets the clock.
/// - Each stretch of editing produces exactly one leave event. Editing the
///   symbol again later makes it dirty again.
/// - **Formatter edits**: a bulk `Edit` (more than one range, or more than
///   one line touched) that is followed by a `Save` of the same file within
///   `formatter_window` is ignored entirely. It doesn't move focus or mark
///   anything dirty. A bulk edit with *no* save in the window (multi-cursor
///   typing, a paste) is a normal edit. So bulk edits must wait out the window
///   before they take effect, and `tick` has to apply them.
pub struct ActivityTracker {
    config: Config,
}

impl ActivityTracker {
    pub fn new(config: Config) -> Self {
        ActivityTracker { config }
    }

    pub fn record(&mut self, signal: Signal) {
        let _ = (signal, &self.config);
        // TODO: implement
    }

    /// Advances time to `now` and returns symbols left since the last tick.
    pub fn tick(&mut self, now: Instant) -> Vec<LeaveEvent> {
        let _ = now;
        Vec::new() // TODO: implement
    }

    /// The symbol I'm currently working in, if known.
    pub fn focus(&self) -> Option<&SymbolId> {
        None // TODO: implement
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const URI: &str = "file:///p/main.rs";

    fn sym(name: &str) -> SymbolId {
        SymbolId {
            uri: URI.into(),
            qualified_name: name.into(),
        }
    }

    /// Test harness: time is "seconds since t0".
    struct T {
        tracker: ActivityTracker,
        t0: Instant,
    }

    impl T {
        fn new() -> Self {
            T {
                tracker: ActivityTracker::new(Config::default()),
                t0: Instant::now(),
            }
        }
        fn at(&self, secs: f64) -> Instant {
            self.t0 + Duration::from_secs_f64(secs)
        }
        fn edit(&mut self, secs: f64, names: &[&str]) {
            let at = self.at(secs);
            self.tracker.record(Signal::Edit {
                uri: URI.into(),
                symbols: names.iter().map(|n| sym(n)).collect(),
                ranges: 1,
                lines_touched: 1,
                at,
            });
        }
        fn bulk_edit(&mut self, secs: f64, names: &[&str]) {
            let at = self.at(secs);
            self.tracker.record(Signal::Edit {
                uri: URI.into(),
                symbols: names.iter().map(|n| sym(n)).collect(),
                ranges: names.len().max(2),
                lines_touched: 8,
                at,
            });
        }
        fn hover(&mut self, secs: f64, name: Option<&str>) {
            let at = self.at(secs);
            self.tracker.record(Signal::Request {
                uri: URI.into(),
                symbol: name.map(sym),
                at,
            });
        }
        fn save(&mut self, secs: f64) {
            let at = self.at(secs);
            self.tracker.record(Signal::Save { uri: URI.into(), at });
        }
        /// Names left at `secs`, sorted.
        fn tick(&mut self, secs: f64) -> Vec<String> {
            let now = self.at(secs);
            let mut names: Vec<_> = self
                .tracker
                .tick(now)
                .into_iter()
                .map(|e| e.symbol.qualified_name)
                .collect();
            names.sort();
            names
        }
    }

    #[test]
    #[ignore = "stub: implement activity::ActivityTracker"]
    fn moving_to_another_symbol_leaves_after_the_timeout() {
        let mut t = T::new();
        t.edit(0.0, &["a"]);
        t.edit(1.0, &["b"]);
        assert_eq!(t.tracker.focus(), Some(&sym("b")));
        assert!(t.tick(5.0).is_empty());
        assert_eq!(t.tick(10.5), ["a"]);
        assert!(t.tick(10.6).is_empty(), "only once");
        assert_eq!(t.tick(11.5), ["b"]);
    }

    #[test]
    #[ignore = "stub: implement activity::ActivityTracker"]
    fn going_quiet_leaves_too() {
        let mut t = T::new();
        t.edit(0.0, &["a"]);
        assert!(t.tick(9.9).is_empty());
        assert_eq!(t.tick(10.1), ["a"]);
    }

    #[test]
    #[ignore = "stub: implement activity::ActivityTracker"]
    fn more_edits_or_a_hover_in_the_symbol_reset_the_clock() {
        let mut t = T::new();
        t.edit(0.0, &["a"]);
        t.edit(5.0, &["a"]);
        assert!(t.tick(12.0).is_empty());
        t.hover(14.0, Some("a"));
        assert!(t.tick(20.0).is_empty());
        assert_eq!(t.tick(24.5), ["a"]);
    }

    #[test]
    #[ignore = "stub: implement activity::ActivityTracker"]
    fn coming_back_before_the_timeout_cancels_the_leave() {
        let mut t = T::new();
        t.edit(0.0, &["a"]);
        t.edit(1.0, &["b"]);
        t.edit(3.0, &["a"]);
        assert!(t.tick(10.5).is_empty(), "a was revisited at 3.0");
        assert_eq!(t.tick(11.5), ["b"]);
        assert_eq!(t.tick(13.5), ["a"]);
    }

    #[test]
    #[ignore = "stub: implement activity::ActivityTracker"]
    fn hovering_never_makes_a_symbol_dirty() {
        let mut t = T::new();
        t.hover(0.0, Some("a"));
        assert_eq!(t.tracker.focus(), Some(&sym("a")));
        assert!(t.tick(60.0).is_empty());
    }

    #[test]
    #[ignore = "stub: implement activity::ActivityTracker"]
    fn editing_outside_any_symbol_clears_focus() {
        let mut t = T::new();
        t.edit(0.0, &["a"]);
        t.edit(1.0, &[]);
        assert_eq!(t.tracker.focus(), None);
        assert_eq!(t.tick(10.5), ["a"]);
    }

    #[test]
    #[ignore = "stub: implement activity::ActivityTracker"]
    fn editing_again_after_a_leave_makes_it_dirty_again() {
        let mut t = T::new();
        t.edit(0.0, &["a"]);
        assert_eq!(t.tick(10.5), ["a"]);
        t.edit(20.0, &["a"]);
        assert_eq!(t.tick(30.5), ["a"]);
    }

    #[test]
    #[ignore = "stub: implement activity::ActivityTracker"]
    fn format_on_save_is_not_me() {
        let mut t = T::new();
        t.edit(0.0, &["a"]);
        t.bulk_edit(1.0, &["a", "b", "c"]);
        t.save(1.02);
        assert!(t.tick(1.5).is_empty());
        assert_eq!(t.tracker.focus(), Some(&sym("a")), "formatter doesn't move focus");
        assert_eq!(t.tick(10.5), ["a"], "formatter didn't touch a's clock or dirty b/c");
        assert!(t.tick(60.0).is_empty());
    }

    #[test]
    #[ignore = "stub: implement activity::ActivityTracker"]
    fn bulk_edit_without_a_save_is_me() {
        // Multi-cursor typing across two functions.
        let mut t = T::new();
        t.bulk_edit(0.0, &["a", "b"]);
        assert!(t.tick(0.5).is_empty());
        assert_eq!(t.tracker.focus(), Some(&sym("b")), "last symbol in the edit");
        assert_eq!(t.tick(10.5), ["a", "b"]);
    }
}
