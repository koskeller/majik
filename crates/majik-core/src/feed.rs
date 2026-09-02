//! Feed logic: multi-selection helpers, double-click detection, carousel windowing, zoom levels
//! and keyboard navigation over the grid.

use std::time::{Duration, Instant};

pub const DOUBLE_CLICK_THRESHOLD: Duration = Duration::from_millis(300);

/// "Favorite" on a mixed selection favorites everything if any item is not
/// yet a favorite; only an all-favorite selection gets unfavorited.
pub fn should_set_favorite(states: impl IntoIterator<Item = bool>) -> bool {
    states.into_iter().any(|is_fav| !is_fav)
}

pub fn is_double_click<T: PartialEq>(last_id: Option<&T>, current: &T, last_time: Instant, now: Instant) -> bool {
    last_id == Some(current) && now.duration_since(last_time) < DOUBLE_CLICK_THRESHOLD
}

/// Indices to keep decoded around the current carousel page.
pub fn visible_range(current: usize, total: usize, buffer: usize) -> std::ops::RangeInclusive<usize> {
    if total == 0 {
        return 0..=0;
    }
    let lower = current.saturating_sub(buffer);
    let upper = (current + buffer).min(total - 1);
    lower..=upper
}

pub fn safe_index(current: usize, count: usize) -> usize {
    current.min(count.saturating_sub(1))
}

/// Zoom levels are minimum tile widths (px); the column count is whatever fits the feed, so a tile
/// never shrinks below its level when the window narrows. At the default window (1100 px, sidebar
/// open) they give 7 / 5 / 4 / 3 / 2 columns.
pub const ZOOM_LEVELS: [u32; 5] = [120, 160, 200, 240, 300];
pub const DEFAULT_ZOOM: u32 = 160;
/// Gutter between grid cells.
pub const GRID_GAP: f32 = 2.;

/// The next larger tile, or `tile` at the top.
pub fn zoom_in(tile: u32) -> u32 {
    ZOOM_LEVELS.iter().copied().find(|&t| t > tile).unwrap_or(tile)
}

/// The next smaller tile, or `tile` at the bottom.
pub fn zoom_out(tile: u32) -> u32 {
    ZOOM_LEVELS.iter().rev().copied().find(|&t| t < tile).unwrap_or(tile)
}

/// A saved level, or the default when it isn't one of [`ZOOM_LEVELS`].
pub fn sanitize_zoom(saved: u32) -> u32 {
    if ZOOM_LEVELS.contains(&saved) {
        saved
    } else {
        DEFAULT_ZOOM
    }
}

/// The most columns whose cells stay at least `tile` wide across `width` with [`GRID_GAP`]
/// gutters; at least one. Cells fill the width, so they end up in `tile..2 * tile + GRID_GAP`.
pub fn columns_for(width: f32, tile: u32) -> usize {
    (((width + GRID_GAP) / (tile as f32 + GRID_GAP)).floor() as usize).max(1)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arrow {
    Left,
    Right,
    Up,
    Down,
}

/// Keyboard navigation over a `columns`-wide grid of `len` items. With no
/// selection any arrow selects the first item; moves clamp at both ends and Down never leaves the
/// grid (it lands on the last item when the row below is short).
pub fn step_selection(current: Option<usize>, arrow: Arrow, columns: usize, len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let Some(current) = current else { return Some(0) };
    let last = len - 1;
    Some(match arrow {
        Arrow::Left => current.saturating_sub(1),
        Arrow::Right => (current + 1).min(last),
        Arrow::Up => current.saturating_sub(columns.max(1)),
        Arrow::Down => (current + columns.max(1)).min(last),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn favorite_rule() {
        assert!(should_set_favorite([true, false]));
        assert!(!should_set_favorite([true, true]));
        assert!(!should_set_favorite(std::iter::empty::<bool>()));
    }

    #[test]
    fn step_selection_clamps() {
        assert_eq!(step_selection(None, Arrow::Down, 5, 0), None);
        assert_eq!(step_selection(None, Arrow::Up, 5, 8), Some(0));
        assert_eq!(step_selection(Some(0), Arrow::Left, 5, 8), Some(0));
        assert_eq!(step_selection(Some(0), Arrow::Up, 5, 8), Some(0));
        assert_eq!(step_selection(Some(1), Arrow::Right, 5, 8), Some(2));
        assert_eq!(step_selection(Some(7), Arrow::Right, 5, 8), Some(7));
        assert_eq!(step_selection(Some(1), Arrow::Down, 5, 8), Some(6));
        assert_eq!(step_selection(Some(4), Arrow::Down, 5, 8), Some(7), "short last row lands on the last item");
        assert_eq!(step_selection(Some(6), Arrow::Up, 5, 8), Some(1));
    }

    #[test]
    fn zoom_steps() {
        assert_eq!(zoom_in(160), 200);
        assert_eq!(zoom_in(200), 240);
        assert_eq!(zoom_in(240), 300);
        assert_eq!(zoom_in(300), 300, "clamped at the largest tile");
        assert_eq!(zoom_out(160), 120);
        assert_eq!(zoom_out(120), 120, "clamped at the smallest tile");
        assert_eq!(sanitize_zoom(120), 120);
        assert_eq!(sanitize_zoom(100), DEFAULT_ZOOM, "unknown levels fall back");
    }

    #[test]
    fn columns_follow_the_width() {
        assert_eq!(columns_for(0., 160), 1, "unmeasured or tiny widths still get a column");
        assert_eq!(columns_for(100., 160), 1);
        // The default 1100 px window with the sidebar open.
        assert_eq!(ZOOM_LEVELS.map(|t| columns_for(866., t)), [7, 5, 4, 3, 2]);
        // Exactly n tiles and n − 1 gutters fit; one pixel less drops a column.
        let exact = 4. * 160. + 3. * GRID_GAP;
        assert_eq!(columns_for(exact, 160), 4);
        assert_eq!(columns_for(exact - 1., 160), 3);
        // From the largest tile up: below that a single column is all there is.
        for width in (300..3000).step_by(7) {
            let width = width as f32;
            for tile in ZOOM_LEVELS {
                let columns = columns_for(width, tile);
                let cell = (width - GRID_GAP * (columns as f32 - 1.)) / columns as f32;
                assert!(cell >= tile as f32, "{width} px at {tile}: {columns} columns give {cell} px cells");
                assert!(cell < 2. * tile as f32 + GRID_GAP, "{width} px at {tile}: another column would still fit");
            }
        }
    }

    #[test]
    fn ranges() {
        assert_eq!(visible_range(0, 3, 1), 0..=1);
        assert_eq!(visible_range(2, 3, 1), 1..=2);
        assert_eq!(safe_index(10, 3), 2);
        assert_eq!(safe_index(0, 0), 0);
    }
}
