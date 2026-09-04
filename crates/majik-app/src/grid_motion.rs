//! The feed grid's motion: cells enter and leave on a snappy spring at 90 % scale, and the media
//! filter and column count ease over 0.25 s, moving and resizing every surviving cell. Only small
//! changes (|Δcount| ≤ 4) animate at all, so the initial load and bulk deletes don't flood the grid.
//!
//! Every id has a [`Place`] (index + column count). When a place changes, the cell's on-screen
//! [`Visual`] (position + size) is interpolated from where it was to where it belongs, so deleting
//! a cell slides its neighbours into the gap and zooming resizes and rearranges in place. Removed
//! cells linger as fading "ghosts" drawn from a render snapshot, since the library drops deleted
//! items immediately. Pure, like `paging.rs`: the view supplies the executor clock and pixel
//! geometry.

use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::time::{Duration, Instant};

use gpui::{SpringConfig, SpringState};
use majik_core::model::GenerationId;

/// The snappy spring: duration 0.5 s, bounce 0.15 (ζ = 0.85, ω₀ = 2π / 0.5 s).
pub const SNAPPY_SPRING: SpringConfig = SpringConfig::new(157.9, 21.4, 1.0);
/// Spring progress within this of 1.0 (and at rest) counts as settled.
pub const SETTLE_EPSILON: f32 = 0.01;
/// Cells enter from / exit to 90 %.
pub const ENTER_SCALE: f32 = 0.9;
/// Removed cells fade out quickly while their neighbours slide into the gap on the spring.
pub const EXIT_DURATION: Duration = Duration::from_millis(150);
/// Ease-in-out for filter and column changes.
pub const REFLOW_DURATION: Duration = Duration::from_millis(250);
/// Changes of more rows than this (initial load, delete-all) apply instantly.
pub const MAX_ANIMATED_DELTA: usize = 4;
/// Thumbnails that arrive while the grid is open fade in over this.
pub const THUMBNAIL_FADE: Duration = Duration::from_millis(200);

/// What caused an id-list / layout change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    /// Items were added / removed in the library: `.snappy`, only for small deltas.
    Library,
    /// The feed or media filter changed: always animated, ease-in-out.
    Filter,
    /// The column count changed: every cell moves and resizes, ease-in-out, nothing fades.
    Zoom,
    /// The feed's width changed the column count: instant, nothing animates (a live drag-resize
    /// would otherwise keep every cell sliding).
    Resize,
    /// The grid switched between rows and masonry: every cell's box changed even where its place
    /// did not, so every survivor moves, ease-in-out, nothing fades. (A masonry cell that learns
    /// its picture's size and shifts the cells below it is a plain `Library` change: their places
    /// are the same, so they jump rather than slide.)
    Layout,
}

/// A cell's slot in a layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Place {
    pub index: usize,
    pub columns: usize,
}

/// Where a cell is drawn: top-left within the grid content and its size, in pixels.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Visual {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Visual {
    pub fn lerp(self, to: Visual, t: f32) -> Visual {
        let mix = |from: f32, to: f32| from + (to - from) * t;
        Visual { x: mix(self.x, to.x), y: mix(self.y, to.y), width: mix(self.width, to.width), height: mix(self.height, to.height) }
    }
}

impl From<majik_core::feed::Slot> for Visual {
    fn from(slot: majik_core::feed::Slot) -> Self {
        Visual { x: slot.x, y: slot.y, width: slot.width, height: slot.height }
    }
}

/// Enter / exit styling: `scale` becomes an inset of the cell's box, `opacity` applies to the box.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CellStyle {
    pub scale: f32,
    pub opacity: f32,
}

impl Default for CellStyle {
    fn default() -> Self {
        Self { scale: 1.0, opacity: 1.0 }
    }
}

#[derive(Clone, Copy, Debug)]
enum Curve {
    Snappy,
    Ease(Duration),
}

impl Curve {
    /// Progress 0..1 (a spring may overshoot slightly) and whether the motion has finished.
    fn sample(self, elapsed: Duration) -> (f32, bool) {
        match self {
            Curve::Snappy => {
                let state = SNAPPY_SPRING.step(SpringState::default(), 1.0, elapsed.as_secs_f32());
                (state.position, SNAPPY_SPRING.is_settled(state, 1.0, SETTLE_EPSILON))
            }
            Curve::Ease(duration) => {
                let t = (elapsed.as_secs_f32() / duration.as_secs_f32()).min(1.0);
                (gpui::ease_in_out(t), t >= 1.0)
            }
        }
    }
}

struct Enter {
    started: Instant,
    curve: Curve,
}

struct Move {
    from: Visual,
    started: Instant,
    curve: Curve,
}

pub struct Ghost<S, K = GenerationId> {
    pub snapshot: S,
    /// Where the cell was when it vanished; it fades there while neighbours slide over it.
    pub from: Visual,
    id: K,
    started: Instant,
    curve: Curve,
}

pub struct GridMotion<S, K = GenerationId> {
    enabled: bool,
    /// The current layout: every id's place. Kept even while disabled so the next change diffs.
    places: HashMap<K, Place>,
    entering: HashMap<K, Enter>,
    moving: HashMap<K, Move>,
    ghosts: Vec<Ghost<S, K>>,
    /// Whether each known id had a thumbnail last time we looked; a `false → true` flip reveals it.
    thumbnails: HashMap<K, bool>,
    revealed: HashMap<K, Instant>,
}

impl<S, K: Clone + Eq + Hash> GridMotion<S, K> {
    pub fn new(enabled: bool) -> Self {
        Self { enabled, places: HashMap::new(), entering: HashMap::new(), moving: HashMap::new(), ghosts: Vec::new(), thumbnails: HashMap::new(), revealed: HashMap::new() }
    }

    /// Reduce-motion switch: while disabled nothing is recorded and any motion in flight is dropped.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.entering.clear();
            self.moving.clear();
            self.ghosts.clear();
            self.revealed.clear();
        }
    }

    /// Commit a new layout (`new` ids over `columns`). `snapshot` yields the render snapshot for a
    /// removed id that was on screen (others vanish silently); `visual` maps a place to pixels in the
    /// current window so moves and ghosts know where cells were.
    pub fn apply(&mut self, new: &[K], columns: usize, change: Change, mut snapshot: impl FnMut(&K) -> Option<S>, visual: impl Fn(Place) -> Visual, now: Instant) {
        let old = std::mem::take(&mut self.places);
        let places: HashMap<K, Place> = new.iter().enumerate().map(|(index, id)| (id.clone(), Place { index, columns })).collect();
        let delta = old.len().abs_diff(new.len());
        // (enter, exit, move) curves, or `None` for an instant change.
        let curves = match change {
            _ if !self.enabled => None,
            Change::Resize => None,
            Change::Library if delta == 0 || delta > MAX_ANIMATED_DELTA => None,
            Change::Library => Some((Curve::Snappy, Curve::Ease(EXIT_DURATION), Curve::Snappy)),
            Change::Filter | Change::Zoom | Change::Layout => Some((Curve::Ease(REFLOW_DURATION), Curve::Ease(REFLOW_DURATION), Curve::Ease(REFLOW_DURATION))),
        };
        match curves {
            Some((enter, exit, shift)) => {
                let new_set: HashSet<&K> = new.iter().collect();
                // An id that comes back while its ghost is still fading re-enters as a live cell.
                self.ghosts.retain(|g| !new_set.contains(&g.id));
                for (id, place) in &places {
                    match old.get(id) {
                        None => {
                            self.moving.remove(id);
                            self.entering.insert(id.clone(), Enter { started: now, curve: enter });
                        }
                        Some(previous) if previous != place || change == Change::Layout => {
                            // A cell retargeted mid-flight continues from where it is now.
                            let from = match self.moving.get(id) {
                                Some(m) => m.from.lerp(visual(*previous), m.curve.sample(now.saturating_duration_since(m.started)).0),
                                None => visual(*previous),
                            };
                            self.moving.insert(id.clone(), Move { from, started: now, curve: shift });
                        }
                        Some(_) => {}
                    }
                }
                for (id, previous) in &old {
                    if places.contains_key(id) {
                        continue;
                    }
                    self.entering.remove(id);
                    self.moving.remove(id);
                    if let Some(snapshot) = snapshot(id) {
                        self.ghosts.push(Ghost { snapshot, from: visual(*previous), id: id.clone(), started: now, curve: exit });
                    }
                }
            }
            None => {
                // Instant: keep only motion that still matches the new layout.
                self.moving.retain(|id, _| old.get(id) == places.get(id));
                self.entering.retain(|id, _| places.contains_key(id));
                self.ghosts.retain(|g| !places.contains_key(&g.id));
            }
        }
        self.places = places;
    }

    pub fn place(&self, id: &K) -> Option<Place> {
        self.places.get(id).copied()
    }

    /// Where to draw `id` this frame given its resting `target`, plus its enter styling.
    pub fn cell(&self, id: &K, target: Visual, now: Instant) -> (Visual, CellStyle) {
        let visual = match self.moving.get(id) {
            Some(m) => m.from.lerp(target, m.curve.sample(now.saturating_duration_since(m.started)).0),
            None => target,
        };
        let style = match self.entering.get(id) {
            Some(enter) => {
                let (p, _) = enter.curve.sample(now.saturating_duration_since(enter.started));
                CellStyle { scale: ENTER_SCALE + (1.0 - ENTER_SCALE) * p, opacity: p.clamp(0.0, 1.0) }
            }
            None => CellStyle::default(),
        };
        (visual, style)
    }

    /// Ids currently sliding to a new place (they may be far from their resting row).
    pub fn moving_ids(&self) -> impl Iterator<Item = &K> {
        self.moving.keys()
    }

    pub fn ghosts(&self) -> impl Iterator<Item = &Ghost<S, K>> {
        self.ghosts.iter()
    }

    pub fn ghost_style(&self, ghost: &Ghost<S, K>, now: Instant) -> CellStyle {
        let (p, _) = ghost.curve.sample(now.saturating_duration_since(ghost.started));
        CellStyle { scale: 1.0 - (1.0 - ENTER_SCALE) * p, opacity: (1.0 - p).clamp(0.0, 1.0) }
    }

    /// Record which ids currently have a thumbnail; one that gains its thumbnail starts a fade-in.
    /// Ids seen for the first time (initial load) never fade.
    pub fn sync_thumbnails<'a>(&mut self, items: impl IntoIterator<Item = (&'a K, bool)>, now: Instant)
    where
        K: 'a,
    {
        let mut thumbnails = HashMap::new();
        for (id, has) in items {
            if self.enabled && has && self.thumbnails.get(id) == Some(&false) {
                self.revealed.insert(id.clone(), now);
            }
            thumbnails.insert(id.clone(), has);
        }
        self.thumbnails = thumbnails;
        self.revealed.retain(|id, _| self.thumbnails.contains_key(id));
    }

    pub fn thumbnail_opacity(&self, id: &K, now: Instant) -> f32 {
        let Some(started) = self.revealed.get(id) else { return 1.0 };
        (now.saturating_duration_since(*started).as_secs_f32() / THUMBNAIL_FADE.as_secs_f32()).clamp(0.0, 1.0)
    }

    /// Drop finished entries. Call once per frame while [`Self::is_animating`].
    pub fn tick(&mut self, now: Instant) {
        self.entering.retain(|_, e| !e.curve.sample(now.saturating_duration_since(e.started)).1);
        self.moving.retain(|_, m| !m.curve.sample(now.saturating_duration_since(m.started)).1);
        self.ghosts.retain(|g| !g.curve.sample(now.saturating_duration_since(g.started)).1);
        self.revealed.retain(|_, started| now.saturating_duration_since(*started) < THUMBNAIL_FADE);
    }

    pub fn is_animating(&self) -> bool {
        !self.entering.is_empty() || !self.moving.is_empty() || !self.ghosts.is_empty() || !self.revealed.is_empty()
    }

    #[cfg(test)]
    pub fn is_entering(&self, id: &K) -> bool {
        self.entering.contains_key(id)
    }

    #[cfg(test)]
    pub fn is_moving(&self, id: &K) -> bool {
        self.moving.contains_key(id)
    }

    #[cfg(test)]
    pub fn ghost_count(&self) -> usize {
        self.ghosts.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CELL: f32 = 100.0;

    fn ids(names: &[&str]) -> Vec<GenerationId> {
        names.iter().map(|n| GenerationId(n.to_string())).collect()
    }

    fn id(name: &str) -> GenerationId {
        GenerationId(name.into())
    }

    fn at(t0: Instant, ms: u64) -> Instant {
        t0 + Duration::from_millis(ms)
    }

    /// A fixed-width grid: cell side shrinks with the column count.
    fn visual(place: Place) -> Visual {
        let size = CELL * 4.0 / place.columns as f32;
        Visual { x: (place.index % place.columns) as f32 * size, y: (place.index / place.columns) as f32 * size, width: size, height: size }
    }

    fn target(m: &GridMotion<&str>, id: &GenerationId) -> Visual {
        visual(m.place(id).expect("placed"))
    }

    /// A grid that already holds `names` over 4 columns, laid out without animation.
    fn grid(names: &[&str]) -> GridMotion<&'static str> {
        let mut m = GridMotion::new(false);
        m.apply(&ids(names), 4, Change::Library, |_| None, visual, Instant::now());
        m.set_enabled(true);
        m
    }

    #[test]
    fn insert_within_delta_enters_from_scale_0_9() {
        let t0 = Instant::now();
        let mut m = grid(&["a"]);
        m.apply(&ids(&["a", "b"]), 4, Change::Library, |_| None, visual, t0);
        assert!(m.is_entering(&id("b")));
        let (visual_b, style) = m.cell(&id("b"), target(&m, &id("b")), t0);
        assert_eq!(visual_b, Visual { x: CELL, y: 0.0, width: CELL, height: CELL }, "enters at its resting place");
        assert_eq!(style, CellStyle { scale: ENTER_SCALE, opacity: 0.0 });
        assert_eq!(m.cell(&id("a"), target(&m, &id("a")), t0).1, CellStyle::default());
        let mid = m.cell(&id("b"), target(&m, &id("b")), at(t0, 100)).1;
        assert!(mid.scale > ENTER_SCALE && mid.opacity > 0.0 && mid.opacity < 1.0, "{mid:?}");
        let mut max_scale: f32 = 0.0;
        for ms in (0..800).step_by(16) {
            max_scale = max_scale.max(m.cell(&id("b"), target(&m, &id("b")), at(t0, ms)).1.scale);
        }
        assert!(max_scale <= 1.01, "snappy barely overshoots: {max_scale}");
        m.tick(at(t0, 800));
        assert!(!m.is_animating());
    }

    #[test]
    fn remove_ghosts_snapshotted_cell_and_slides_neighbours_into_the_gap() {
        let t0 = Instant::now();
        let mut m = grid(&["a", "b", "c", "d", "e"]);
        // "b" was on screen (has a snapshot), "d" was not.
        m.apply(&ids(&["a", "c", "e"]), 4, Change::Library, |id| (id.0 == "b").then_some("b-snapshot"), visual, t0);
        assert_eq!(m.ghost_count(), 1);
        let ghost = m.ghosts().next().unwrap();
        assert_eq!(ghost.snapshot, "b-snapshot");
        assert_eq!(ghost.from, Visual { x: CELL, y: 0.0, width: CELL, height: CELL }, "fades where it was");
        assert_eq!(m.ghost_style(ghost, t0), CellStyle::default());
        let late = m.ghost_style(ghost, at(t0, 100));
        assert!(late.opacity < 0.5 && late.scale < 1.0, "{late:?}");
        // "c" slides from column 2 to column 1; "a" stays put; "e" wraps up from row 1.
        assert!(m.is_moving(&id("c")) && m.is_moving(&id("e")) && !m.is_moving(&id("a")));
        assert_eq!(m.cell(&id("c"), target(&m, &id("c")), t0).0, Visual { x: 2.0 * CELL, y: 0.0, width: CELL, height: CELL });
        let mid = m.cell(&id("c"), target(&m, &id("c")), at(t0, 120)).0;
        assert!(mid.x > CELL && mid.x < 2.0 * CELL, "{mid:?}");
        assert_eq!(m.cell(&id("e"), target(&m, &id("e")), t0).0, Visual { x: 0.0, y: CELL, width: CELL, height: CELL });
        m.tick(at(t0, 150));
        assert_eq!(m.ghost_count(), 0, "the fade is quick");
        assert!(m.is_moving(&id("c")), "the slide is still settling");
        m.tick(at(t0, 800));
        assert!(!m.is_animating());
        assert_eq!(m.cell(&id("c"), target(&m, &id("c")), at(t0, 800)).0, Visual { x: CELL, y: 0.0, width: CELL, height: CELL });
    }

    #[test]
    fn bulk_change_is_instant() {
        let t0 = Instant::now();
        let mut m = grid(&[]);
        let many = ids(&["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"]);
        m.apply(&many, 4, Change::Library, |_| Some("x"), visual, t0);
        assert!(!m.is_animating());
        m.apply(&ids(&["a", "b", "c"]), 4, Change::Library, |_| Some("x"), visual, t0);
        assert!(!m.is_animating());
        // Exactly four is still animated.
        let mut m = grid(&["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"]);
        m.apply(&ids(&["a", "b", "c", "d", "e", "f"]), 4, Change::Library, |_| Some("x"), visual, t0);
        assert_eq!(m.ghost_count(), 4);
    }

    #[test]
    fn equal_count_replacement_is_instant() {
        let t0 = Instant::now();
        let mut m = grid(&["a", "b"]);
        m.apply(&ids(&["a", "c"]), 4, Change::Library, |_| Some("x"), visual, t0);
        assert!(!m.is_animating());
        assert_eq!(m.place(&id("c")), Some(Place { index: 1, columns: 4 }));
    }

    #[test]
    fn zoom_moves_and_resizes_every_cell_without_fading() {
        let t0 = Instant::now();
        let mut m = grid(&["a", "b", "c", "d", "e"]);
        m.apply(&ids(&["a", "b", "c", "d", "e"]), 2, Change::Zoom, |_| Some("x"), visual, t0);
        assert_eq!(m.ghost_count(), 0);
        assert!(!m.is_entering(&id("a")));
        assert!(["a", "b", "c", "d", "e"].iter().all(|n| m.is_moving(&id(n))));
        // "c" was (2, 0) at side 100; it rests at (0, 1) at side 200.
        let (start, style) = m.cell(&id("c"), target(&m, &id("c")), t0);
        assert_eq!(start, Visual { x: 2.0 * CELL, y: 0.0, width: CELL, height: CELL });
        assert_eq!(style, CellStyle::default(), "no fade on zoom");
        let (mid, _) = m.cell(&id("c"), target(&m, &id("c")), at(t0, 125));
        assert!((mid.width - 150.0).abs() < 1.0 && (mid.height - 150.0).abs() < 1.0 && mid.x < 2.0 * CELL && mid.y > 0.0, "{mid:?}");
        m.tick(at(t0, 250));
        assert!(!m.is_animating());
        assert_eq!(m.cell(&id("c"), target(&m, &id("c")), at(t0, 250)).0, Visual { x: 0.0, y: 2.0 * CELL, width: 2.0 * CELL, height: 2.0 * CELL });
    }

    /// A width change re-places every cell without motion, and the next zoom animates from the
    /// resized layout rather than the one before it.
    #[test]
    fn resize_replaces_cells_instantly() {
        let t0 = Instant::now();
        let mut m = grid(&["a", "b", "c", "d", "e"]);
        m.apply(&ids(&["a", "b", "c", "d", "e"]), 2, Change::Resize, |_| Some("x"), visual, t0);
        assert!(!m.is_animating());
        assert_eq!(m.ghost_count(), 0);
        assert!(["a", "b", "c", "d", "e"].iter().all(|n| !m.is_moving(&id(n)) && !m.is_entering(&id(n))));
        assert_eq!(m.place(&id("c")), Some(Place { index: 2, columns: 2 }));
        m.apply(&ids(&["a", "b", "c", "d", "e"]), 4, Change::Zoom, |_| Some("x"), visual, t0);
        // "c" starts from its 2-column place (0, 1) at side 200, not the original 4-column one.
        assert_eq!(m.cell(&id("c"), target(&m, &id("c")), t0).0, Visual { x: 0.0, y: 2.0 * CELL, width: 2.0 * CELL, height: 2.0 * CELL });
    }

    #[test]
    fn retarget_mid_flight_continues_from_the_current_position() {
        let t0 = Instant::now();
        let mut m = grid(&["a", "b", "c", "d", "e"]);
        m.apply(&ids(&["a", "b", "c", "d", "e"]), 2, Change::Zoom, |_| None, visual, t0);
        let midway = m.cell(&id("c"), target(&m, &id("c")), at(t0, 125)).0;
        m.apply(&ids(&["a", "b", "c", "d", "e"]), 4, Change::Zoom, |_| None, visual, at(t0, 125));
        assert_eq!(m.cell(&id("c"), target(&m, &id("c")), at(t0, 125)).0, midway, "no jump when zooming back");
    }

    #[test]
    fn filter_change_ignores_delta_rule_and_uses_reflow_curve() {
        let t0 = Instant::now();
        let mut m = grid(&["a", "b", "c", "d", "e", "f", "g", "h"]);
        m.apply(&ids(&["x", "y"]), 4, Change::Filter, |_| Some("x"), visual, t0);
        assert_eq!(m.ghost_count(), 8);
        assert!(m.is_entering(&id("x")));
        m.tick(at(t0, 249));
        assert!(m.is_animating());
        m.tick(at(t0, 250));
        assert!(!m.is_animating());
    }

    #[test]
    fn returning_id_drops_its_ghost() {
        let t0 = Instant::now();
        let mut m = grid(&["a", "b"]);
        m.apply(&ids(&["a"]), 4, Change::Filter, |_| Some("x"), visual, t0);
        assert_eq!(m.ghost_count(), 1);
        m.apply(&ids(&["a", "b"]), 4, Change::Filter, |_| Some("x"), visual, at(t0, 50));
        assert_eq!(m.ghost_count(), 0);
        assert!(m.is_entering(&id("b")));
    }

    #[test]
    fn disabled_records_nothing_but_tracks_places() {
        let t0 = Instant::now();
        let mut m: GridMotion<&str> = GridMotion::new(false);
        m.apply(&ids(&["a", "b"]), 4, Change::Library, |_| Some("x"), visual, t0);
        m.apply(&ids(&["a", "b"]), 2, Change::Zoom, |_| Some("x"), visual, t0);
        m.sync_thumbnails([(&id("a"), false)], t0);
        m.sync_thumbnails([(&id("a"), true)], t0);
        assert!(!m.is_animating());
        assert_eq!(m.place(&id("b")), Some(Place { index: 1, columns: 2 }));
        assert_eq!(m.thumbnail_opacity(&id("a"), t0), 1.0);
        // Disabling mid-flight drops everything.
        let mut m = grid(&["a"]);
        m.apply(&ids(&["a", "b"]), 4, Change::Library, |_| Some("x"), visual, t0);
        assert!(m.is_animating());
        m.set_enabled(false);
        assert!(!m.is_animating());
    }

    #[test]
    fn thumbnail_reveal_fades_in_over_200ms() {
        let t0 = Instant::now();
        let mut m = grid(&[]);
        let a = id("a");
        // First sighting with a thumbnail (initial load): no fade.
        m.sync_thumbnails([(&a, true)], t0);
        assert_eq!(m.thumbnail_opacity(&a, t0), 1.0);
        let b = id("b");
        m.sync_thumbnails([(&a, true), (&b, false)], t0);
        m.sync_thumbnails([(&a, true), (&b, true)], at(t0, 10));
        assert_eq!(m.thumbnail_opacity(&b, at(t0, 10)), 0.0);
        assert!((m.thumbnail_opacity(&b, at(t0, 110)) - 0.5).abs() < 0.01);
        assert!(m.is_animating());
        m.tick(at(t0, 210));
        assert_eq!(m.thumbnail_opacity(&b, at(t0, 210)), 1.0);
        assert!(!m.is_animating());
    }

    #[test]
    fn tick_prunes_finished_entries() {
        let t0 = Instant::now();
        let mut m = grid(&["a", "b"]);
        m.apply(&ids(&["a", "c"]), 4, Change::Filter, |_| Some("x"), visual, t0);
        assert!(m.is_animating());
        m.tick(at(t0, 5_000));
        assert!(!m.is_animating());
        assert_eq!(m.ghost_count(), 0);
        assert!(!m.is_entering(&id("c")));
    }
}
