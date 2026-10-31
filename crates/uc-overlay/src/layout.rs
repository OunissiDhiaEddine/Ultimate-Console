//! Where everything goes, in device-independent pixels (1/96 inch). Shared by
//! the renderer, the DWM thumbnail placement and mouse hit-testing.

use crate::model::Tab;

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    pub fn right(&self) -> f32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }

    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }

    pub fn inset(&self, d: f32) -> Rect {
        Rect::new(
            self.x + d,
            self.y + d,
            (self.w - 2.0 * d).max(0.0),
            (self.h - 2.0 * d).max(0.0),
        )
    }

    /// The largest rect with the aspect ratio `src_w:src_h` that fits inside
    /// `self`, centred. Used to letterbox DWM thumbnails.
    pub fn fit(&self, src_w: f32, src_h: f32) -> Rect {
        if src_w <= 0.0 || src_h <= 0.0 {
            return *self;
        }
        let scale = (self.w / src_w).min(self.h / src_h);
        let (w, h) = (src_w * scale, src_h * scale);
        Rect::new(
            self.x + (self.w - w) / 2.0,
            self.y + (self.h - h) / 2.0,
            w,
            h,
        )
    }

    pub fn scaled(&self, s: f32) -> Rect {
        Rect::new(self.x * s, self.y * s, self.w * s, self.h * s)
    }
}

const MAX_CARD_W: f32 = 1120.0;
const MAX_CARD_H: f32 = 700.0;
const PAD: f32 = 28.0;
const HEADER_H: f32 = 64.0;
const FOOTER_H: f32 = 56.0;
const GAP: f32 = 18.0;
const LABEL_H: f32 = 30.0;
pub const ROW_H: f32 = 64.0;

/// One task tile: the thumbnail area and the title under it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tile {
    pub index: usize,
    pub outer: Rect,
    pub thumb: Rect,
    pub label: Rect,
}

/// What a mouse click landed on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    Tab(usize),
    Tile(usize),
    Card,
    Outside,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Layout {
    pub screen: Rect,
    pub card: Rect,
    pub header: Rect,
    pub body: Rect,
    pub footer: Rect,
    pub columns: usize,
    tile_w: f32,
    tile_h: f32,
}

impl Layout {
    /// Layout for a screen of `w` × `h` DIPs.
    pub fn new(w: f32, h: f32) -> Self {
        let screen = Rect::new(0.0, 0.0, w, h);
        let cw = MAX_CARD_W.min(w * 0.86);
        let ch = MAX_CARD_H.min(h * 0.86);
        let card = Rect::new((w - cw) / 2.0, (h - ch) / 2.0, cw, ch);
        let inner = card.inset(PAD);
        let header = Rect::new(inner.x, inner.y, inner.w, HEADER_H);
        let footer = Rect::new(inner.x, inner.bottom() - FOOTER_H, inner.w, FOOTER_H);
        let body = Rect::new(
            inner.x,
            header.bottom() + GAP / 2.0,
            inner.w,
            (footer.y - header.bottom() - GAP).max(0.0),
        );
        let columns = if body.w >= 900.0 {
            4
        } else if body.w >= 560.0 {
            3
        } else {
            2
        };
        let tile_w = (body.w - GAP * (columns as f32 - 1.0)) / columns as f32;
        let tile_h = tile_w * 9.0 / 16.0 + LABEL_H;
        Self {
            screen,
            card,
            header,
            body,
            footer,
            columns,
            tile_w,
            tile_h,
        }
    }

    pub fn tab(&self, i: usize) -> Rect {
        let w = 150.0;
        Rect::new(
            self.header.x + i as f32 * w,
            self.header.y,
            w,
            self.header.h,
        )
    }

    /// Whole rows of tiles that fit in the body (at least one).
    pub fn visible_rows(&self) -> usize {
        (((self.body.h + GAP) / (self.tile_h + GAP)).floor() as usize).max(1)
    }

    /// Tiles on screen for `count` tasks with the grid scrolled to `scroll_row`.
    pub fn tiles(&self, count: usize, scroll_row: usize) -> Vec<Tile> {
        let first = scroll_row * self.columns;
        let last = (first + self.visible_rows() * self.columns).min(count);
        (first..last)
            .map(|index| {
                let rel = index - first;
                let (row, col) = (rel / self.columns, rel % self.columns);
                let outer = Rect::new(
                    self.body.x + col as f32 * (self.tile_w + GAP),
                    self.body.y + row as f32 * (self.tile_h + GAP),
                    self.tile_w,
                    self.tile_h,
                );
                let thumb = Rect::new(outer.x, outer.y, outer.w, outer.h - LABEL_H).inset(6.0);
                let label = Rect::new(
                    outer.x + 8.0,
                    outer.bottom() - LABEL_H,
                    outer.w - 16.0,
                    LABEL_H,
                );
                Tile {
                    index,
                    outer,
                    thumb,
                    label,
                }
            })
            .collect()
    }

    /// Rows of the volume panel that fit.
    pub fn volume_rows_visible(&self) -> usize {
        ((self.body.h / ROW_H).floor() as usize).max(1)
    }

    pub fn hit(&self, x: f32, y: f32, tab: Tab, count: usize, scroll_row: usize) -> Hit {
        if !self.card.contains(x, y) {
            return Hit::Outside;
        }
        if let Some(i) = (0..Tab::ALL.len()).find(|&i| self.tab(i).contains(x, y)) {
            return Hit::Tab(i);
        }
        if tab == Tab::Tasks
            && let Some(t) = self
                .tiles(count, scroll_row)
                .into_iter()
                .find(|t| t.outer.contains(x, y))
        {
            return Hit::Tile(t.index);
        }
        Hit::Card
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn card_is_centred_and_capped() {
        let l = Layout::new(3840.0, 2160.0);
        assert_eq!(l.card.w, MAX_CARD_W);
        assert_eq!(l.card.h, MAX_CARD_H);
        assert_eq!(l.card.x * 2.0 + l.card.w, 3840.0);
        let small = Layout::new(800.0, 600.0);
        assert!(small.card.w < 800.0 && small.card.right() < 800.0);
        assert!(small.columns < l.columns);
    }

    #[test]
    fn tiles_stay_inside_body() {
        for (w, h) in [(1920.0, 1080.0), (1280.0, 720.0), (800.0, 600.0)] {
            let l = Layout::new(w, h);
            let tiles = l.tiles(50, 0);
            assert_eq!(tiles.len(), l.visible_rows() * l.columns);
            for t in &tiles {
                assert!(t.outer.x >= l.body.x - 0.01 && t.outer.right() <= l.body.right() + 0.01);
                assert!(t.outer.bottom() <= l.body.bottom() + 0.01, "{w}x{h} {t:?}");
            }
        }
    }

    #[test]
    fn tiles_follow_scroll() {
        let l = Layout::new(1920.0, 1080.0);
        let tiles = l.tiles(10, 1);
        assert_eq!(tiles[0].index, l.columns);
        assert_eq!(tiles[0].outer.y, l.body.y);
        assert_eq!(l.tiles(3, 0).len(), 3);
    }

    #[test]
    fn fit_letterboxes() {
        let r = Rect::new(0.0, 0.0, 160.0, 90.0);
        assert_eq!(r.fit(100.0, 100.0), Rect::new(35.0, 0.0, 90.0, 90.0));
        assert_eq!(r.fit(320.0, 90.0), Rect::new(0.0, 22.5, 160.0, 45.0));
        assert_eq!(r.fit(0.0, 10.0), r);
    }

    #[test]
    fn hit_testing() {
        let l = Layout::new(1920.0, 1080.0);
        assert_eq!(l.hit(5.0, 5.0, Tab::Tasks, 4, 0), Hit::Outside);
        let t = l.tab(1);
        assert_eq!(l.hit(t.x + 5.0, t.y + 5.0, Tab::Tasks, 4, 0), Hit::Tab(1));
        let tile = l.tiles(4, 0)[2];
        let (x, y) = (tile.outer.x + 4.0, tile.outer.y + 4.0);
        assert_eq!(l.hit(x, y, Tab::Tasks, 4, 0), Hit::Tile(2));
        assert_eq!(l.hit(x, y, Tab::Volume, 4, 0), Hit::Card);
    }
}
