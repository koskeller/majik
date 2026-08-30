//! The detail carousel's paging: one item per swipe, plus an animated slide for key/button
//! navigation.
//!
//! The detail view is modelled as a horizontal strip of viewport-wide slots: slot `k` sits at
//! `x = (k - index) * width + offset`. `offset` is the strip's displacement from rest, so `+width`
//! means the previous item is still fully on screen. The resting target is always `0`; a critically
//! damped spring carries velocity across retargets, which is what makes mashing →, reversing, or
//! swiping mid-slide seamless. This module is pure so it can be unit-tested without a window; the
//! view feeds it `TouchPhase`s, pixel deltas and a clock.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use gpui::{Point, SpringConfig, SpringState, TouchPhase};

/// Critically damped, ω₀ = 2π / 0.3 s — visually an ~0.3 s ease-out.
pub const SLIDE_SPRING: SpringConfig = SpringConfig::new(439.0, 41.9, 1.0);
/// Offset (px) under which a slide is considered finished.
pub const SETTLE_EPSILON: f32 = 0.5;
/// Distance (px) a gesture must travel before it is locked to an axis.
pub const AXIS_LOCK_DISTANCE: f32 = 6.0;
/// Fraction of the width a drag must cover to commit on release.
pub const COMMIT_FRACTION: f32 = 0.3;
/// Release velocity (px/s) that commits a flick regardless of distance.
pub const COMMIT_VELOCITY: f32 = 300.0;
/// A flick only commits once the strip has actually moved this far (px).
pub const FLICK_MIN_DISTANCE: f32 = 8.0;
/// Release velocity (px/s) is clamped to this before being handed to the spring.
pub const MAX_RELEASE_VELOCITY: f32 = 4000.0;
/// UIScrollView's rubber-band constant.
pub const RUBBER_BAND_COEFFICIENT: f32 = 0.55;
/// Repeated key presses never displace the strip beyond this many widths.
pub const MAX_REBASE_FACTOR: f32 = 1.5;
/// Only samples this recent count towards the release velocity.
pub const VELOCITY_WINDOW: Duration = Duration::from_millis(100);
/// On backends that never send phases, a gesture that goes quiet for this long is released.
pub const GESTURE_TIMEOUT: Duration = Duration::from_millis(150);
/// With phases, `Ended` is the release and resting fingers must not page; this only unsticks a
/// gesture the system cancelled (macOS delivers `Cancelled` as a plain `Moved`).
pub const STALE_GESTURE_TIMEOUT: Duration = Duration::from_millis(1000);

/// What the view knows about the strip: its width and whether the current item is first / last.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Edges {
    pub width: f32,
    pub at_start: bool,
    pub at_end: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Prev,
    Next,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Axis {
    Horizontal,
    Vertical,
}

struct Gesture {
    axis: Option<Axis>,
    /// Accumulated finger travel; content follows the fingers, so `raw.x < 0` reveals the next item.
    raw: Point<f32>,
    /// `(time, raw.x)` samples inside [`VELOCITY_WINDOW`], newest last.
    samples: VecDeque<(Instant, f32)>,
}

#[derive(Default)]
pub struct Paging {
    spring: SpringState,
    animating: bool,
    last_tick: Option<Instant>,
    gesture: Option<Gesture>,
    /// Set once the backend has sent a `Started`/`Ended`. Until then a `Moved` without a gesture
    /// opens one implicitly; afterwards it is momentum and ignored.
    phases_seen: bool,
}

impl Paging {
    /// Horizontal displacement of the strip from rest, in px.
    pub fn offset(&self) -> f32 {
        self.spring.position
    }

    pub fn is_animating(&self) -> bool {
        self.animating
    }

    /// A horizontal gesture is engaged and the strip follows the fingers.
    #[cfg(test)]
    pub fn is_dragging(&self) -> bool {
        self.gesture.as_ref().is_some_and(|g| g.axis == Some(Axis::Horizontal))
    }

    /// A gesture is open (possibly not yet axis-locked) and needs a release or a timeout.
    pub fn has_gesture(&self) -> bool {
        self.gesture.is_some()
    }

    /// How long an open gesture may stay quiet before the view should call [`Self::finish`].
    pub fn quiet_timeout(&self) -> Duration {
        if self.phases_seen { STALE_GESTURE_TIMEOUT } else { GESTURE_TIMEOUT }
    }

    /// Advance the spring toward rest. Call once per frame while [`Self::is_animating`].
    pub fn tick(&mut self, now: Instant) {
        if !self.animating {
            return;
        }
        let dt = self.last_tick.map(|t| now.saturating_duration_since(t).as_secs_f32()).unwrap_or(0.0);
        self.last_tick = Some(now);
        self.spring = SLIDE_SPRING.step(self.spring, 0.0, dt);
        if SLIDE_SPRING.is_settled(self.spring, 0.0, SETTLE_EPSILON) {
            self.spring = SpringState::default();
            self.animating = false;
            self.last_tick = None;
        }
    }

    /// Start a slide after the view has already moved `index` by `step`: the strip is displaced so
    /// the old item stays where it was, then springs to rest. Cancels any gesture in progress.
    pub fn navigate(&mut self, step: Step, width: f32, now: Instant) {
        self.gesture = None;
        let sign = match step {
            Step::Next => 1.0,
            Step::Prev => -1.0,
        };
        let limit = MAX_REBASE_FACTOR * width;
        self.spring.position = (self.spring.position + sign * width).clamp(-limit, limit);
        self.start_animating(now);
    }

    /// Feed a precise (trackpad) scroll event. Returns the page change the view must apply to
    /// `index`; the strip is already re-based so the change is visually seamless.
    pub fn scroll(&mut self, phase: TouchPhase, delta: Point<f32>, edges: Edges, now: Instant) -> Option<Step> {
        match phase {
            TouchPhase::Started => {
                self.phases_seen = true;
                self.begin_gesture();
                None
            }
            TouchPhase::Moved => {
                if self.gesture.is_none() {
                    if self.phases_seen {
                        return None;
                    }
                    self.begin_gesture();
                }
                self.drag(delta, edges, now);
                None
            }
            TouchPhase::Ended => {
                self.phases_seen = true;
                self.finish(edges, now)
            }
            TouchPhase::Cancelled => {
                self.gesture = None;
                self.spring.velocity = 0.0;
                self.start_animating(now);
                None
            }
        }
    }

    /// Release the gesture (the `Ended` path, also used by the view's quiet-gesture timeout).
    pub fn finish(&mut self, edges: Edges, now: Instant) -> Option<Step> {
        let mut gesture = self.gesture.take()?;
        if gesture.axis != Some(Axis::Horizontal) {
            self.start_animating(now);
            return None;
        }
        gesture.prune_samples(now);
        let velocity = gesture.velocity();
        let width = edges.width;
        let position = self.spring.position;
        let step = if position <= -COMMIT_FRACTION * width || (velocity <= -COMMIT_VELOCITY && position < -FLICK_MIN_DISTANCE) {
            Some(Step::Next)
        } else if position >= COMMIT_FRACTION * width || (velocity >= COMMIT_VELOCITY && position > FLICK_MIN_DISTANCE) {
            Some(Step::Prev)
        } else {
            None
        };
        let past_edge = (edges.at_end && position < 0.0) || (edges.at_start && position > 0.0);
        let step = if past_edge { None } else { step };
        match step {
            Some(Step::Next) => self.spring.position += width,
            Some(Step::Prev) => self.spring.position -= width,
            None => {}
        }
        self.spring.velocity = if past_edge { 0.0 } else { velocity.clamp(-MAX_RELEASE_VELOCITY, MAX_RELEASE_VELOCITY) };
        self.start_animating(now);
        step
    }

    /// Drop everything and rest at zero (the current item changed under us).
    pub fn reset(&mut self) {
        self.spring = SpringState::default();
        self.animating = false;
        self.last_tick = None;
        self.gesture = None;
    }

    fn begin_gesture(&mut self) {
        // Freeze whatever slide was running: the strip now follows the fingers from where it is.
        self.spring.velocity = 0.0;
        self.animating = false;
        self.last_tick = None;
        self.gesture = Some(Gesture { axis: None, raw: Point::new(self.spring.position, 0.0), samples: VecDeque::new() });
    }

    fn drag(&mut self, delta: Point<f32>, edges: Edges, now: Instant) {
        let Some(gesture) = self.gesture.as_mut() else { return };
        gesture.raw.x += delta.x;
        gesture.raw.y += delta.y;
        if gesture.axis.is_none() && gesture.raw.x.hypot(gesture.raw.y) >= AXIS_LOCK_DISTANCE {
            gesture.axis = Some(if gesture.raw.x.abs() >= gesture.raw.y.abs() { Axis::Horizontal } else { Axis::Vertical });
        }
        if gesture.axis != Some(Axis::Horizontal) {
            return;
        }
        let x = gesture.raw.x;
        self.spring.position = if edges.at_end && x < 0.0 {
            -rubber_band(-x, edges.width)
        } else if edges.at_start && x > 0.0 {
            rubber_band(x, edges.width)
        } else {
            x
        };
        gesture.samples.push_back((now, x));
        gesture.prune_samples(now);
    }

    fn start_animating(&mut self, now: Instant) {
        if self.spring.position == 0.0 && self.spring.velocity == 0.0 {
            self.animating = false;
            self.last_tick = None;
        } else {
            self.animating = true;
            self.last_tick = Some(now);
        }
    }
}

impl Gesture {
    fn prune_samples(&mut self, now: Instant) {
        while self.samples.front().is_some_and(|(t, _)| now.saturating_duration_since(*t) > VELOCITY_WINDOW) {
            self.samples.pop_front();
        }
    }

    /// Release velocity in px/s over the retained window; zero when the fingers were still.
    fn velocity(&self) -> f32 {
        let (Some((t0, x0)), Some((t1, x1))) = (self.samples.front(), self.samples.back()) else {
            return 0.0;
        };
        let dt = t1.saturating_duration_since(*t0).as_secs_f32();
        if dt <= 0.0 {
            return 0.0;
        }
        (x1 - x0) / dt
    }
}

/// How far the strip actually moves when dragged `distance` past an edge (UIScrollView's curve).
pub fn rubber_band(distance: f32, width: f32) -> f32 {
    if width <= 0.0 {
        return 0.0;
    }
    (1.0 - 1.0 / (distance * RUBBER_BAND_COEFFICIENT / width + 1.0)) * width
}

/// Slots to render: every slot within two of `index` that intersects the viewport, plus the direct
/// neighbours regardless of visibility so their images are decoded before a swipe reveals them.
pub fn visible_slots(index: usize, count: usize, offset: f32, width: f32) -> Vec<usize> {
    let mut slots = Vec::with_capacity(5);
    for k in index.saturating_sub(2)..=index.saturating_add(2) {
        if k >= count {
            break;
        }
        let distance = k as isize - index as isize;
        let x = distance as f32 * width + offset;
        let visible = x < width && x + width > 0.0;
        if visible || distance.abs() <= 1 {
            slots.push(k);
        }
    }
    slots
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: f32 = 800.0;

    fn at(t0: Instant, ms: u64) -> Instant {
        t0 + Duration::from_millis(ms)
    }

    fn mid() -> Edges {
        Edges { width: W, at_start: false, at_end: false }
    }

    fn settle(paging: &mut Paging, t0: Instant, from_ms: u64) -> Vec<f32> {
        let mut trace = Vec::new();
        let mut ms = from_ms;
        while paging.is_animating() && ms < from_ms + 2000 {
            ms += 16;
            paging.tick(at(t0, ms));
            trace.push(paging.offset());
        }
        trace
    }

    fn swipe(paging: &mut Paging, t0: Instant, edges: Edges, dx: f32, steps: u64, step_ms: u64) -> Option<Step> {
        paging.scroll(TouchPhase::Started, Point::new(0.0, 0.0), edges, at(t0, 0));
        for i in 1..=steps {
            paging.scroll(TouchPhase::Moved, Point::new(dx / steps as f32, 0.0), edges, at(t0, i * step_ms));
        }
        paging.scroll(TouchPhase::Ended, Point::new(0.0, 0.0), edges, at(t0, steps * step_ms))
    }

    #[test]
    fn navigate_next_starts_at_plus_width_and_settles() {
        let t0 = Instant::now();
        let mut paging = Paging::default();
        paging.navigate(Step::Next, W, t0);
        assert_eq!(paging.offset(), W);
        assert!(paging.is_animating());
        let trace = settle(&mut paging, t0, 0);
        assert!(!paging.is_animating());
        assert_eq!(paging.offset(), 0.0);
        assert!(trace.windows(2).all(|w| w[1] <= w[0]), "ease-out from rest never overshoots");
        assert!(trace.len() * 16 <= 600, "settles within 600 ms, took {} ms", trace.len() * 16);
    }

    #[test]
    fn navigate_prev_starts_at_minus_width() {
        let t0 = Instant::now();
        let mut paging = Paging::default();
        paging.navigate(Step::Prev, W, t0);
        assert_eq!(paging.offset(), -W);
    }

    #[test]
    fn repeated_navigate_rebases_and_clamps() {
        let t0 = Instant::now();
        let mut paging = Paging::default();
        paging.navigate(Step::Next, W, t0);
        paging.tick(at(t0, 16));
        paging.navigate(Step::Next, W, at(t0, 16));
        assert!(paging.offset() <= MAX_REBASE_FACTOR * W);
        assert!(paging.offset() > W, "second press adds to the remaining displacement");
        for i in 0..10 {
            paging.navigate(Step::Next, W, at(t0, 32 + i));
            assert!(paging.offset() <= MAX_REBASE_FACTOR * W);
        }
        // Reversing subtracts and keeps animating toward rest.
        paging.navigate(Step::Prev, W, at(t0, 50));
        assert!(paging.offset() < MAX_REBASE_FACTOR * W);
        assert!(paging.is_animating());
    }

    #[test]
    fn drag_past_commit_fraction_commits() {
        let t0 = Instant::now();
        let mut paging = Paging::default();
        // Slow drag: 0.4 W over 800 ms, well below the flick velocity.
        let step = swipe(&mut paging, t0, mid(), -0.4 * W, 40, 20);
        assert_eq!(step, Some(Step::Next));
        assert!((paging.offset() - 0.6 * W).abs() < 1.0, "re-based so the strip doesn't jump: {}", paging.offset());
        settle(&mut paging, t0, 800);
        assert_eq!(paging.offset(), 0.0);
    }

    #[test]
    fn drag_right_commits_prev() {
        let t0 = Instant::now();
        let mut paging = Paging::default();
        assert_eq!(swipe(&mut paging, t0, mid(), 0.5 * W, 40, 20), Some(Step::Prev));
        assert!((paging.offset() + 0.5 * W).abs() < 1.0);
    }

    #[test]
    fn short_drag_snaps_back() {
        let t0 = Instant::now();
        let mut paging = Paging::default();
        let step = swipe(&mut paging, t0, mid(), -0.1 * W, 40, 20);
        assert_eq!(step, None);
        assert!(paging.is_animating());
        settle(&mut paging, t0, 800);
        assert_eq!(paging.offset(), 0.0);
    }

    #[test]
    fn flick_commits_on_velocity() {
        let t0 = Instant::now();
        let mut paging = Paging::default();
        // 40 px in 30 ms ≈ 1333 px/s.
        assert_eq!(swipe(&mut paging, t0, mid(), -40.0, 3, 10), Some(Step::Next));
        assert!(paging.offset() > 0.9 * W);
    }

    #[test]
    fn hold_then_release_does_not_flick() {
        let t0 = Instant::now();
        let mut paging = Paging::default();
        let edges = mid();
        paging.scroll(TouchPhase::Started, Point::new(0.0, 0.0), edges, at(t0, 0));
        paging.scroll(TouchPhase::Moved, Point::new(-40.0, 0.0), edges, at(t0, 10));
        // Fingers rest for half a second before lifting.
        assert_eq!(paging.scroll(TouchPhase::Ended, Point::new(0.0, 0.0), edges, at(t0, 500)), None);
    }

    #[test]
    fn rubber_band_at_end() {
        let t0 = Instant::now();
        let mut paging = Paging::default();
        let edges = Edges { width: W, at_start: false, at_end: true };
        paging.scroll(TouchPhase::Started, Point::new(0.0, 0.0), edges, at(t0, 0));
        paging.scroll(TouchPhase::Moved, Point::new(-W, 0.0), edges, at(t0, 100));
        assert!(paging.offset() < 0.0 && paging.offset().abs() < RUBBER_BAND_COEFFICIENT * W);
        assert_eq!(paging.scroll(TouchPhase::Ended, Point::new(0.0, 0.0), edges, at(t0, 110)), None);
        settle(&mut paging, t0, 110);
        assert_eq!(paging.offset(), 0.0);
    }

    #[test]
    fn rubber_band_at_start() {
        let t0 = Instant::now();
        let mut paging = Paging::default();
        let edges = Edges { width: W, at_start: true, at_end: false };
        assert_eq!(swipe(&mut paging, t0, edges, W, 10, 10), None);
    }

    #[test]
    fn vertical_gesture_locks_and_ignores_horizontal() {
        let t0 = Instant::now();
        let mut paging = Paging::default();
        let edges = mid();
        paging.scroll(TouchPhase::Started, Point::new(0.0, 0.0), edges, at(t0, 0));
        paging.scroll(TouchPhase::Moved, Point::new(1.0, 20.0), edges, at(t0, 10));
        paging.scroll(TouchPhase::Moved, Point::new(-400.0, 0.0), edges, at(t0, 20));
        assert_eq!(paging.offset(), 0.0);
        assert!(!paging.is_dragging());
        assert_eq!(paging.scroll(TouchPhase::Ended, Point::new(0.0, 0.0), edges, at(t0, 30)), None);
    }

    #[test]
    fn moved_without_started_is_ignored_once_phases_seen() {
        let t0 = Instant::now();
        let mut paging = Paging::default();
        let edges = mid();
        assert_eq!(swipe(&mut paging, t0, edges, -0.5 * W, 10, 10), Some(Step::Next));
        let after_release = paging.offset();
        // Momentum arrives as phase-less Moved events.
        for i in 0..10 {
            paging.scroll(TouchPhase::Moved, Point::new(-30.0, 0.0), edges, at(t0, 200 + i * 10));
        }
        assert_eq!(paging.offset(), after_release);
        assert!(paging.is_animating());
    }

    #[test]
    fn moved_without_phases_opens_implicit_gesture() {
        let t0 = Instant::now();
        let mut paging = Paging::default();
        let edges = mid();
        for i in 0..10 {
            paging.scroll(TouchPhase::Moved, Point::new(-40.0, 0.0), edges, at(t0, i * 10));
        }
        assert!(paging.is_dragging());
        assert!((paging.offset() + 400.0).abs() < 0.01);
        assert_eq!(paging.finish(edges, at(t0, 250)), Some(Step::Next));
    }

    #[test]
    fn cancelled_snaps_back() {
        let t0 = Instant::now();
        let mut paging = Paging::default();
        let edges = mid();
        paging.scroll(TouchPhase::Started, Point::new(0.0, 0.0), edges, at(t0, 0));
        paging.scroll(TouchPhase::Moved, Point::new(-300.0, 0.0), edges, at(t0, 10));
        assert_eq!(paging.scroll(TouchPhase::Cancelled, Point::new(0.0, 0.0), edges, at(t0, 20)), None);
        assert!(paging.is_animating());
        settle(&mut paging, t0, 20);
        assert_eq!(paging.offset(), 0.0);
    }

    #[test]
    fn quiet_timeout_is_short_only_without_phases() {
        let t0 = Instant::now();
        let mut paging = Paging::default();
        assert_eq!(paging.quiet_timeout(), GESTURE_TIMEOUT);
        paging.scroll(TouchPhase::Started, Point::new(0.0, 0.0), mid(), at(t0, 0));
        assert_eq!(paging.quiet_timeout(), STALE_GESTURE_TIMEOUT);
    }

    #[test]
    fn timeout_finishes_stale_gesture() {
        let t0 = Instant::now();
        let mut paging = Paging::default();
        let edges = mid();
        paging.scroll(TouchPhase::Started, Point::new(0.0, 0.0), edges, at(t0, 0));
        paging.scroll(TouchPhase::Moved, Point::new(-0.5 * W, 0.0), edges, at(t0, 10));
        assert_eq!(paging.finish(edges, at(t0, 200)), Some(Step::Next));
        assert!(!paging.is_dragging());
        assert_eq!(paging.finish(edges, at(t0, 210)), None, "nothing left to finish");
    }

    #[test]
    fn swipe_mid_slide_freezes_then_continues() {
        let t0 = Instant::now();
        let mut paging = Paging::default();
        paging.navigate(Step::Next, W, t0);
        paging.tick(at(t0, 50));
        let frozen = paging.offset();
        assert!(frozen > 0.0 && frozen < W);
        let edges = mid();
        paging.scroll(TouchPhase::Started, Point::new(0.0, 0.0), edges, at(t0, 60));
        assert!(!paging.is_animating());
        assert_eq!(paging.offset(), frozen);
        paging.scroll(TouchPhase::Moved, Point::new(-10.0, 0.0), edges, at(t0, 70));
        assert!((paging.offset() - (frozen - 10.0)).abs() < 0.01, "strip follows the fingers from where it was");
        // The old item is still mostly on screen, so releasing snaps to it — the nearest page — and
        // the view steps back; the strip is re-based so nothing jumps.
        assert_eq!(paging.scroll(TouchPhase::Ended, Point::new(0.0, 0.0), edges, at(t0, 500)), Some(Step::Prev));
        assert!((paging.offset() - (frozen - 10.0 - W)).abs() < 0.01);
        settle(&mut paging, t0, 500);
        assert_eq!(paging.offset(), 0.0);
    }

    #[test]
    fn navigate_cancels_gesture() {
        let t0 = Instant::now();
        let mut paging = Paging::default();
        let edges = mid();
        paging.scroll(TouchPhase::Started, Point::new(0.0, 0.0), edges, at(t0, 0));
        paging.scroll(TouchPhase::Moved, Point::new(-100.0, 0.0), edges, at(t0, 10));
        paging.navigate(Step::Next, W, at(t0, 20));
        assert!(!paging.is_dragging());
        // Later Moved events belong to the abandoned gesture and are dropped.
        paging.scroll(TouchPhase::Moved, Point::new(-100.0, 0.0), edges, at(t0, 30));
        assert!((paging.offset() - (W - 100.0)).abs() < 0.01);
    }

    #[test]
    fn reset_rests_at_zero() {
        let t0 = Instant::now();
        let mut paging = Paging::default();
        paging.navigate(Step::Next, W, t0);
        paging.reset();
        assert_eq!(paging.offset(), 0.0);
        assert!(!paging.is_animating() && !paging.is_dragging());
    }

    #[test]
    fn rubber_band_is_bounded_and_monotonic() {
        assert_eq!(rubber_band(0.0, W), 0.0);
        assert!(rubber_band(W, W) < rubber_band(10.0 * W, W));
        assert!(rubber_band(100.0 * W, W) < W);
        assert_eq!(rubber_band(100.0, 0.0), 0.0);
    }

    #[test]
    fn visible_slots_always_prefetches_neighbours() {
        assert_eq!(visible_slots(5, 10, 0.0, W), vec![4, 5, 6]);
        assert_eq!(visible_slots(0, 10, 0.0, W), vec![0, 1]);
        assert_eq!(visible_slots(9, 10, 0.0, W), vec![8, 9]);
        assert_eq!(visible_slots(0, 1, 0.0, W), vec![0]);
        assert_eq!(visible_slots(0, 0, 0.0, W), Vec::<usize>::new());
        // Unmeasured stage: neighbours still listed, nothing else.
        assert_eq!(visible_slots(5, 10, 0.0, 0.0), vec![4, 5, 6]);
    }

    #[test]
    fn visible_slots_covers_rebased_offsets() {
        // After two quick "next" presses the strip sits at +1.5 W: index-2 is on screen.
        assert_eq!(visible_slots(5, 10, 1.5 * W, W), vec![3, 4, 5, 6]);
        assert_eq!(visible_slots(5, 10, -1.5 * W, W), vec![4, 5, 6, 7]);
        assert_eq!(visible_slots(5, 10, 0.5 * W, W), vec![4, 5, 6]);
    }
}
