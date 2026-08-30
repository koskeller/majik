//! The detail view's open/close transition: the feed cell's box grows into the stage's fitted
//! image rect, and shrinks back into its cell on close, as Photos does. Pure and clock-driven like
//! `paging.rs` so the view tests can step it headlessly.

use std::time::{Duration, Instant};

use gpui::{point, size, Bounds, Pixels};

/// The cell ⇄ stage travel.
pub const DURATION: Duration = Duration::from_millis(280);
/// Used when there is no cell to travel between (item off screen, audio, still generating).
pub const FADE_DURATION: Duration = Duration::from_millis(150);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Open,
    Close,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Shape {
    /// The box travels between the feed cell and the image's resting rect on the stage, both in
    /// window coordinates.
    Geometry { cell: Bounds<Pixels>, stage: Bounds<Pixels> },
    /// Nothing to travel between: the whole detail crossfades.
    Fade,
}

#[derive(Clone, Copy, Debug)]
pub struct Morph {
    direction: Direction,
    shape: Shape,
    started: Instant,
}

impl Morph {
    /// A geometry morph when both rects are known, otherwise a fade.
    pub fn new(direction: Direction, cell: Option<Bounds<Pixels>>, stage: Option<Bounds<Pixels>>, now: Instant) -> Self {
        let shape = match (cell, stage) {
            (Some(cell), Some(stage)) if cell.size.width > Pixels::ZERO && stage.size.width > Pixels::ZERO => Shape::Geometry { cell, stage },
            _ => Shape::Fade,
        };
        Self { direction, shape, started: now }
    }

    pub fn direction(&self) -> Direction {
        self.direction
    }

    #[cfg(test)]
    pub fn shape(&self) -> Shape {
        self.shape
    }

    pub fn duration(&self) -> Duration {
        match self.shape {
            Shape::Geometry { .. } => DURATION,
            Shape::Fade => FADE_DURATION,
        }
    }

    pub fn is_done(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.started) >= self.duration()
    }

    /// How far open the detail is: 0 = the cell, 1 = the stage. Eased; `Close` runs 1 → 0.
    pub fn progress(&self, now: Instant) -> f32 {
        let elapsed = now.saturating_duration_since(self.started).as_secs_f32();
        let t = (elapsed / self.duration().as_secs_f32()).clamp(0.0, 1.0);
        let eased = ease_out_cubic(t);
        match self.direction {
            Direction::Open => eased,
            Direction::Close => 1.0 - eased,
        }
    }

    /// Where the travelling box is right now (`None` for a fade).
    pub fn frame(&self, now: Instant) -> Option<Bounds<Pixels>> {
        let Shape::Geometry { cell, stage } = self.shape else { return None };
        Some(lerp_bounds(cell, stage, self.progress(now)))
    }
}

fn ease_out_cubic(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

fn lerp(a: Pixels, b: Pixels, t: f32) -> Pixels {
    a + (b - a) * t
}

pub fn lerp_bounds(a: Bounds<Pixels>, b: Bounds<Pixels>, t: f32) -> Bounds<Pixels> {
    Bounds {
        origin: point(lerp(a.origin.x, b.origin.x, t), lerp(a.origin.y, b.origin.y, t)),
        size: size(lerp(a.size.width, b.size.width, t), lerp(a.size.height, b.size.height, t)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::px;

    fn rect(x: f32, y: f32, w: f32, h: f32) -> Bounds<Pixels> {
        Bounds { origin: point(px(x), px(y)), size: size(px(w), px(h)) }
    }

    #[test]
    fn open_travels_from_cell_to_stage() {
        let t0 = Instant::now();
        let m = Morph::new(Direction::Open, Some(rect(10., 20., 100., 100.)), Some(rect(210., 120., 500., 300.)), t0);
        assert_eq!(m.progress(t0), 0.0);
        assert_eq!(m.frame(t0), Some(rect(10., 20., 100., 100.)));
        assert!(!m.is_done(t0));
        let mid = m.progress(t0 + DURATION / 2);
        assert!(mid > 0.5 && mid < 1.0, "ease-out is ahead of linear at the midpoint: {mid}");
        let end = t0 + DURATION;
        assert_eq!(m.progress(end), 1.0);
        assert_eq!(m.frame(end), Some(rect(210., 120., 500., 300.)));
        assert!(m.is_done(end));
        assert_eq!(m.progress(end + DURATION), 1.0, "clamped after the end");
    }

    #[test]
    fn close_runs_backwards() {
        let t0 = Instant::now();
        let m = Morph::new(Direction::Close, Some(rect(0., 0., 10., 10.)), Some(rect(0., 0., 20., 20.)), t0);
        assert_eq!(m.progress(t0), 1.0);
        assert_eq!(m.frame(t0), Some(rect(0., 0., 20., 20.)));
        assert_eq!(m.progress(t0 + DURATION), 0.0);
        assert_eq!(m.frame(t0 + DURATION), Some(rect(0., 0., 10., 10.)));
    }

    #[test]
    fn missing_or_empty_rect_falls_back_to_fade() {
        let t0 = Instant::now();
        let fade = Morph::new(Direction::Open, None, Some(rect(0., 0., 20., 20.)), t0);
        assert_eq!(fade.shape(), Shape::Fade);
        assert_eq!(fade.frame(t0), None);
        assert_eq!(fade.duration(), FADE_DURATION);
        assert!(fade.is_done(t0 + FADE_DURATION));
        let unmeasured = Morph::new(Direction::Close, Some(rect(0., 0., 10., 10.)), Some(Bounds::default()), t0);
        assert_eq!(unmeasured.shape(), Shape::Fade);
    }
}
