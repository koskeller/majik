//! Feed logic: multi-selection helpers, double-click detection, carousel windowing, zoom levels,
//! the grid's geometry ([`Layout`]: square cells in rows, or masonry columns) and keyboard
//! navigation over it.

use std::ops::{Range, RangeInclusive};
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

/// Width / height a masonry cell may have. Wide enough for 21:9 and tall enough for 9:21, which
/// is every ratio the catalogs offer; beyond it a cell is cropped, so a 1:10 import can't take a
/// whole column and its decoded pixels stay bounded (the image cache's budget counts on this).
pub const MASONRY_RATIO_RANGE: RangeInclusive<f32> = (1. / 3.)..=3.;

/// The width / height a masonry cell gets for a picture: square while the size is unknown
/// (audio, a generation still running, a file that went missing, a broken probe), otherwise the
/// picture's own, clamped to [`MASONRY_RATIO_RANGE`].
pub fn masonry_ratio(ratio: Option<f32>) -> f32 {
    match ratio {
        Some(ratio) if ratio.is_finite() && ratio > 0. => ratio.clamp(*MASONRY_RATIO_RANGE.start(), *MASONRY_RATIO_RANGE.end()),
        _ => 1.,
    }
}

/// Columns whose bottoms are within this fraction of the cell width of the shortest one count as
/// equally short, and the leftmost of them takes the next cell. Portraits of nearly the same
/// shape (9:16 next to 0.558) then keep their reading order instead of skipping a column that
/// ended a few pixels lower; a real difference — a landscape next to a portrait — is still a
/// third of a cell or more, so the shortest column wins as before.
pub const MASONRY_TIE: f32 = 0.25;

/// A cell's box in the grid content, in pixels, and the column it sits in.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Slot {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub column: usize,
}

impl Slot {
    pub fn bottom(&self) -> f32 {
        self.y + self.height
    }
}

/// Where every cell of the feed is drawn, for one width and column count: either rows of square
/// cells, or masonry — cells at their picture's shape, each dropped into the column that is
/// shortest at its turn (or a column to its left that is nearly as short, see [`MASONRY_TIE`]).
/// A cell's `y` is not sorted along the index, but each cell's `floor` — the shortest column's
/// bottom when it was placed, which only ever rises — is, and `y` sits within the tie of it, so
/// the cells at a scroll offset are found by binary search on the floors; what a scroll can't see
/// by `y` alone is a tall cell that started above it, which is why the tallest cell's height is
/// kept.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Layout {
    slots: Vec<Slot>,
    /// Per slot: the lowest column bottom (plus the gap) when it was placed; non-decreasing.
    floors: Vec<f32>,
    columns: usize,
    height: f32,
    max_height: f32,
    /// How far above its floor a cell can sit: [`MASONRY_TIE`] of the cell width, 0 in rows.
    tie: f32,
    masonry: bool,
}

impl Layout {
    /// Square cells `cell` on a side in rows of `columns`, `gap` apart.
    pub fn uniform(count: usize, columns: usize, cell: f32, gap: f32) -> Self {
        let columns = columns.max(1);
        let pitch = cell + gap;
        let slots: Vec<Slot> = (0..count).map(|index| Slot { x: (index % columns) as f32 * pitch, y: (index / columns) as f32 * pitch, width: cell, height: cell, column: index % columns }).collect();
        let floors = slots.iter().map(|slot| slot.y).collect();
        let rows = count.div_ceil(columns);
        let height = if rows == 0 { 0. } else { rows as f32 * pitch - gap };
        Self { slots, floors, columns, height, max_height: if count == 0 { 0. } else { cell }, tie: 0., masonry: false }
    }

    /// Cells `cell` wide at their pictures' shapes (see [`masonry_ratio`]) in `columns` columns
    /// `gap` apart, each in the column that is shortest when its turn comes, or the leftmost of
    /// those within [`MASONRY_TIE`] of it.
    pub fn masonry(ratios: impl IntoIterator<Item = Option<f32>>, columns: usize, cell: f32, gap: f32) -> Self {
        let columns = columns.max(1);
        let tie = cell * MASONRY_TIE;
        let mut bottoms = vec![0.0f32; columns];
        let mut slots = Vec::new();
        let mut floors = Vec::new();
        let mut max_height = 0.0f32;
        for ratio in ratios {
            let shortest = bottoms.iter().copied().fold(f32::INFINITY, f32::min);
            let column = bottoms.iter().position(|bottom| *bottom <= shortest + tie).unwrap_or(0);
            let below = |bottom: f32| if bottom > 0. { bottom + gap } else { 0. };
            let y = below(bottoms[column]);
            let height = cell / masonry_ratio(ratio);
            slots.push(Slot { x: column as f32 * (cell + gap), y, width: cell, height, column });
            floors.push(below(shortest));
            bottoms[column] = y + height;
            max_height = max_height.max(height);
        }
        let height = bottoms.iter().copied().fold(0.0f32, f32::max);
        Self { slots, floors, columns, height, max_height, tie, masonry: true }
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    pub fn columns(&self) -> usize {
        self.columns
    }

    /// Height of the whole content, to the bottom of the lowest cell.
    pub fn height(&self) -> f32 {
        self.height
    }

    /// The tallest cell: how far above a scroll offset a cell can start and still reach it.
    pub fn max_height(&self) -> f32 {
        self.max_height
    }

    pub fn is_masonry(&self) -> bool {
        self.masonry
    }

    pub fn slot(&self, index: usize) -> Option<Slot> {
        self.slots.get(index).copied()
    }

    /// The first cell placed once the shortest column reached `y`: in rows, the first cell of the
    /// row starting at or below `y`; in masonry, a cell whose top is within the tie of `y` or
    /// below it (the last cell when every one starts above).
    pub fn first_index_at(&self, y: f32) -> usize {
        self.floors.partition_point(|floor| *floor < y).min(self.slots.len().saturating_sub(1))
    }

    /// The indices of every cell that overlaps `top..bottom`, plus any cell in between that
    /// doesn't (the range is contiguous, so a caller iterating it still has to check each cell).
    pub fn visible_range(&self, top: f32, bottom: f32) -> Range<usize> {
        // A cell sits at most `tie` below its floor and is at most `max_height` tall.
        let first = self.floors.partition_point(|floor| floor + self.tie + self.max_height <= top);
        let last = self.floors.partition_point(|floor| *floor < bottom);
        first..last.max(first)
    }

    /// Keyboard navigation over this layout: rows of cells step by the column count; masonry
    /// keeps Left / Right in index order and moves Up / Down to the nearest cell in the same
    /// column, staying put at the column's ends.
    pub fn step_selection(&self, current: Option<usize>, arrow: Arrow) -> Option<usize> {
        if !self.masonry {
            return step_selection(current, arrow, self.columns, self.len());
        }
        if self.is_empty() {
            return None;
        }
        let Some(current) = current else { return Some(0) };
        let current = current.min(self.len() - 1);
        let column = self.slots[current].column;
        Some(match arrow {
            Arrow::Left => current.saturating_sub(1),
            Arrow::Right => (current + 1).min(self.len() - 1),
            Arrow::Up => (0..current).rev().find(|&index| self.slots[index].column == column).unwrap_or(current),
            Arrow::Down => (current + 1..self.len()).find(|&index| self.slots[index].column == column).unwrap_or(current),
        })
    }
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

    const CELL: f32 = 100.;
    const GAP: f32 = 2.;

    /// A deterministic spread of shapes: portrait, square, landscape and a few unknowns.
    fn ratios(count: usize) -> Vec<Option<f32>> {
        (0..count).map(|i| match i % 7 { 0 => Some(0.5), 1 => Some(1.), 2 => Some(2.), 3 => None, 4 => Some(0.75), 5 => Some(16. / 9.), _ => Some(9. / 16.) }).collect()
    }

    #[test]
    fn uniform_layout_matches_the_row_grid() {
        let layout = Layout::uniform(7, 3, CELL, GAP);
        assert_eq!(layout.len(), 7);
        assert!(!layout.is_masonry());
        assert_eq!(layout.slot(0), Some(Slot { x: 0., y: 0., width: CELL, height: CELL, column: 0 }));
        assert_eq!(layout.slot(4), Some(Slot { x: CELL + GAP, y: CELL + GAP, width: CELL, height: CELL, column: 1 }));
        assert_eq!(layout.slot(6).map(|s| s.column), Some(0));
        assert_eq!(layout.slot(7), None);
        assert_eq!(layout.height(), 3. * CELL + 2. * GAP, "three rows, two gutters");
        assert_eq!(layout.max_height(), CELL);
        assert_eq!(Layout::uniform(0, 3, CELL, GAP), Layout { columns: 3, ..Default::default() });
    }

    #[test]
    fn masonry_places_each_cell_in_the_shortest_column() {
        // 2:1, 1:2, 1:1 over two columns: the third goes under the short landscape, not the tall portrait.
        let layout = Layout::masonry([Some(2.), Some(0.5), Some(1.), Some(1.)], 2, CELL, GAP);
        assert!(layout.is_masonry());
        assert_eq!(layout.slot(0), Some(Slot { x: 0., y: 0., width: CELL, height: CELL / 2., column: 0 }));
        assert_eq!(layout.slot(1), Some(Slot { x: CELL + GAP, y: 0., width: CELL, height: 2. * CELL, column: 1 }));
        assert_eq!(layout.slot(2), Some(Slot { x: 0., y: CELL / 2. + GAP, width: CELL, height: CELL, column: 0 }));
        assert_eq!(layout.slot(3), Some(Slot { x: 0., y: 1.5 * CELL + 2. * GAP, width: CELL, height: CELL, column: 0 }), "column 0 is still shorter than the portrait");
        assert_eq!(layout.height(), 2.5 * CELL + 2. * GAP, "the lowest bottom");
        assert_eq!(layout.max_height(), 2. * CELL);
    }

    /// Two columns of portraits a few pixels apart in height read row by row: the third cell
    /// goes under the first even though the second column ended a hair lower.
    #[test]
    fn masonry_near_ties_keep_the_reading_order() {
        let (a, b) = (Some(0.5625), Some(0.5581));
        let layout = Layout::masonry([b, a, a, a, b, a], 2, CELL, GAP);
        assert_eq!((0..6).map(|i| layout.slot(i).unwrap().column).collect::<Vec<_>>(), [0, 1, 0, 1, 0, 1]);
        let taller = CELL / 0.5581;
        assert_eq!(layout.slot(2).unwrap().y, taller + GAP, "under the first, which is the taller");
        assert_eq!(layout.slot(3).unwrap().y, CELL / 0.5625 + GAP);
        // A difference of a quarter of the cell width is where the shortest column takes over.
        let just_shorter = Some(CELL / (CELL / 1. - CELL * MASONRY_TIE + 0.5));
        let layout = Layout::masonry([Some(1.), just_shorter, Some(1.)], 2, CELL, GAP);
        assert_eq!(layout.slot(2).unwrap().column, 0, "within the tie: leftmost");
        let clearly_shorter = Some(CELL / (CELL - CELL * MASONRY_TIE - 0.5));
        let layout = Layout::masonry([Some(1.), clearly_shorter, Some(1.)], 2, CELL, GAP);
        assert_eq!(layout.slot(2).unwrap().column, 1, "beyond the tie: the shortest");
        // A landscape next to a portrait is never a tie.
        let layout = Layout::masonry([Some(0.5), Some(2.), Some(1.)], 2, CELL, GAP);
        assert_eq!(layout.slot(2).unwrap().column, 1);
    }

    #[test]
    fn masonry_ties_go_to_the_leftmost_column() {
        let layout = Layout::masonry([Some(1.), Some(1.), Some(1.), Some(1.)], 3, CELL, GAP);
        assert_eq!((0..4).map(|i| layout.slot(i).unwrap().column).collect::<Vec<_>>(), [0, 1, 2, 0]);
        assert_eq!(layout.slot(3).unwrap().y, CELL + GAP);
    }

    #[test]
    fn masonry_floors_are_monotone_and_columns_never_overlap() {
        for columns in 1..=6 {
            let layout = Layout::masonry(ratios(500), columns, CELL, GAP);
            let mut bottoms = vec![0.0f32; columns];
            let mut last_floor = 0.;
            for index in 0..500 {
                let slot = layout.slot(index).unwrap();
                let floor = layout.floors[index];
                assert!(floor >= last_floor, "{columns} columns: cell {index}'s floor is above cell {}'s", index - 1);
                last_floor = floor;
                assert!(slot.y >= floor && slot.y <= floor + CELL * MASONRY_TIE + GAP, "{columns} columns: cell {index} at {} is not within the tie of its floor {floor}", slot.y);
                assert!(slot.y >= bottoms[slot.column], "{columns} columns: cell {index} overlaps the one above");
                bottoms[slot.column] = slot.bottom();
                assert_eq!(slot.x, slot.column as f32 * (CELL + GAP));
            }
            assert_eq!(layout.height(), bottoms.iter().copied().fold(0., f32::max));
        }
    }

    #[test]
    fn masonry_treats_unknown_and_extreme_ratios_as_square_or_clamped() {
        assert_eq!(masonry_ratio(None), 1.);
        assert_eq!(masonry_ratio(Some(0.)), 1.);
        assert_eq!(masonry_ratio(Some(f32::NAN)), 1.);
        assert_eq!(masonry_ratio(Some(f32::INFINITY)), 1.);
        assert_eq!(masonry_ratio(Some(10.)), 3.);
        assert_eq!(masonry_ratio(Some(0.1)), 1. / 3.);
        assert_eq!(masonry_ratio(Some(21. / 9.)), 21. / 9., "every catalog ratio passes through");
        assert_eq!(masonry_ratio(Some(9. / 21.)), 9. / 21.);
        let layout = Layout::masonry([None, Some(0.1)], 1, CELL, GAP);
        assert_eq!(layout.slot(0).unwrap().height, CELL);
        assert_eq!(layout.slot(1).unwrap().height, 3. * CELL);
    }

    #[test]
    fn visible_range_covers_every_slot_that_overlaps() {
        let layout = Layout::masonry(ratios(300), 4, CELL, GAP);
        for top in (0..layout.height() as usize).step_by(37) {
            let (top, bottom) = (top as f32, top as f32 + 500.);
            let range = layout.visible_range(top, bottom);
            for index in 0..layout.len() {
                let slot = layout.slot(index).unwrap();
                let overlaps = slot.bottom() > top && slot.y < bottom;
                assert!(!overlaps || range.contains(&index), "cell {index} ({slot:?}) overlaps {top}..{bottom} but {range:?} misses it");
            }
            // Tight at the bottom, and at the top no wider than the tallest cell.
            assert!(range.end == layout.len() || layout.slot(range.end).unwrap().y >= bottom);
            assert!(range.start == 0 || layout.slot(range.start - 1).unwrap().y + layout.max_height() <= top);
        }
        assert_eq!(Layout::default().visible_range(0., 100.), 0..0);
    }

    #[test]
    fn first_index_at_follows_the_scroll_top() {
        let layout = Layout::uniform(10, 3, CELL, GAP);
        assert_eq!(layout.first_index_at(0.), 0);
        assert_eq!(layout.first_index_at(1.), 3, "a row scrolled off by a pixel is behind us");
        assert_eq!(layout.first_index_at(CELL + GAP), 3, "exactly at a row's top");
        assert_eq!(layout.first_index_at(10_000.), 9, "past the end: the last cell");
        assert_eq!(Layout::default().first_index_at(0.), 0);
        // Masonry: the cell placed once the shortest column reached `y`, wherever it sits.
        let layout = Layout::masonry([Some(2.), Some(0.5), Some(1.), Some(1.)], 2, CELL, GAP);
        assert_eq!(layout.first_index_at(0.), 0);
        assert_eq!(layout.first_index_at(1.), 2, "column 0's half-height cell had ended");
        assert_eq!(layout.first_index_at(CELL), 3);
    }

    #[test]
    fn masonry_step_selection_moves_within_the_column() {
        // Columns: 0 → [0, 2, 3], 1 → [1] (the portrait fills its column).
        let layout = Layout::masonry([Some(2.), Some(0.5), Some(1.), Some(1.)], 2, CELL, GAP);
        assert_eq!(layout.step_selection(None, Arrow::Down), Some(0));
        assert_eq!(layout.step_selection(Some(0), Arrow::Down), Some(2));
        assert_eq!(layout.step_selection(Some(2), Arrow::Down), Some(3));
        assert_eq!(layout.step_selection(Some(3), Arrow::Down), Some(3), "the column's end");
        assert_eq!(layout.step_selection(Some(3), Arrow::Up), Some(2));
        assert_eq!(layout.step_selection(Some(1), Arrow::Up), Some(1), "alone in its column");
        assert_eq!(layout.step_selection(Some(1), Arrow::Down), Some(1));
        assert_eq!(layout.step_selection(Some(1), Arrow::Left), Some(0));
        assert_eq!(layout.step_selection(Some(1), Arrow::Right), Some(2), "index order, whatever the column");
        assert_eq!(layout.step_selection(Some(3), Arrow::Right), Some(3));
        assert_eq!(layout.step_selection(Some(9), Arrow::Up), Some(2), "a stale index is clamped first");
        assert_eq!(Layout::masonry([], 2, CELL, GAP).step_selection(Some(0), Arrow::Down), None);
    }

    #[test]
    fn uniform_step_selection_matches_the_free_function() {
        let layout = Layout::uniform(8, 5, CELL, GAP);
        for current in [None, Some(0), Some(1), Some(4), Some(6), Some(7)] {
            for arrow in [Arrow::Left, Arrow::Right, Arrow::Up, Arrow::Down] {
                assert_eq!(layout.step_selection(current, arrow), step_selection(current, arrow, 5, 8), "{current:?} {arrow:?}");
            }
        }
    }
}
