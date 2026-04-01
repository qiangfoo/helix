//! Extension trait for ratatui's Buffer providing custom methods needed by helix.
//!
//! These methods were originally part of helix-tui's Buffer implementation and
//! are ported here as an extension trait on ratatui's Buffer.

use helix_core::unicode::width::UnicodeWidthStr;
use helix_view::graphics::Style;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use std::cmp::min;
use helix_core::unicode::segmentation::UnicodeSegmentation;

pub trait BufferExt {
    /// Clear an area in the buffer with a default style.
    fn clear_with(&mut self, area: Rect, style: Style);

    /// Clear an area in the buffer (reset all cells).
    fn clear_area(&mut self, area: Rect);

    /// Tells whether the global (x, y) coordinates are inside the Buffer's area.
    fn in_bounds(&self, x: u16, y: u16) -> bool;

    /// Print at most the first `width` characters of a string if enough space is available
    /// until the end of the line. If `ellipsis` is true appends a `…` at the end of truncated
    /// lines. If `truncate_start` is `true`, truncate the beginning of the string instead.
    #[allow(clippy::too_many_arguments)]
    fn set_string_truncated(
        &mut self,
        x: u16,
        y: u16,
        string: &str,
        width: usize,
        style: impl Fn(usize) -> Style,
        ellipsis: bool,
        truncate_start: bool,
    ) -> (u16, u16);

    /// Print at most the first `width` characters of a string if enough space is available
    /// until the end of the line.
    /// If `truncate_start` is true, adds a `…` at the beginning of truncated lines.
    /// If `truncate_end` is true, adds a `…` at the end of truncated lines.
    #[allow(clippy::too_many_arguments)]
    fn set_string_anchored(
        &mut self,
        x: u16,
        y: u16,
        truncate_start: bool,
        truncate_end: bool,
        string: &str,
        width: usize,
        style: impl Fn(usize) -> Style,
    ) -> (u16, u16);
}

impl BufferExt for Buffer {
    fn clear_with(&mut self, area: Rect, style: Style) {
        let style: ratatui::style::Style = style.into();
        for x in area.left()..area.right() {
            for y in area.top()..area.bottom() {
                if let Some(cell) = self.cell_mut((x, y)) {
                    cell.reset();
                    cell.set_style(style);
                }
            }
        }
    }

    fn clear_area(&mut self, area: Rect) {
        for x in area.left()..area.right() {
            for y in area.top()..area.bottom() {
                if let Some(cell) = self.cell_mut((x, y)) {
                    cell.reset();
                }
            }
        }
    }

    fn in_bounds(&self, x: u16, y: u16) -> bool {
        x >= self.area.left()
            && x < self.area.right()
            && y >= self.area.top()
            && y < self.area.bottom()
    }

    fn set_string_truncated(
        &mut self,
        x: u16,
        y: u16,
        string: &str,
        width: usize,
        style: impl Fn(usize) -> Style,
        ellipsis: bool,
        truncate_start: bool,
    ) -> (u16, u16) {
        if !self.in_bounds(x, y) || width == 0 {
            return (x, y);
        }

        let mut index = ((y - self.area.y) as usize) * (self.area.width as usize)
            + ((x - self.area.x) as usize);
        let mut x_offset = x as usize;
        let width = if ellipsis { width - 1 } else { width };
        let graphemes = string.grapheme_indices(true);
        let max_offset = min(self.area.right() as usize, width.saturating_add(x as usize));
        if !truncate_start {
            for (byte_offset, s) in graphemes {
                let width = s.width();
                if width == 0 {
                    continue;
                }
                if width > max_offset.saturating_sub(x_offset) {
                    break;
                }

                self[(x_offset as u16, y)].set_symbol(s);
                self[(x_offset as u16, y)].set_style(ratatui::style::Style::from(style(byte_offset)));
                // Reset following cells if multi-width
                for i in 1..width {
                    let cx = (x_offset + i) as u16;
                    if let Some(cell) = self.cell_mut((cx, y)) {
                        cell.reset();
                    }
                }
                index += width;
                x_offset += width;
            }
            if ellipsis && x_offset - (x as usize) < string.width() {
                self[(x_offset as u16, y)].set_symbol("…");
            }
        } else {
            let start_x = x;
            let mut start_index = ((y - self.area.y) as usize) * (self.area.width as usize)
                + ((start_x - self.area.x) as usize);
            index = ((y - self.area.y) as usize) * (self.area.width as usize)
                + ((max_offset as u16 - self.area.x) as usize);

            let content_width = string.width();
            let truncated = content_width > width;
            if ellipsis && truncated {
                self[(start_x, y)].set_symbol("…");
                start_index += 1;
            }
            if !truncated {
                index -= width - content_width;
            }
            for (byte_offset, s) in graphemes.rev() {
                let width = s.width();
                if width == 0 {
                    continue;
                }
                let start = index - width;
                if start < start_index {
                    break;
                }
                let sx = self.area.x + (start % self.area.width as usize) as u16;
                self[(sx, y)].set_symbol(s);
                self[(sx, y)].set_style(ratatui::style::Style::from(style(byte_offset)));
                for i in 1..width {
                    let cx = sx + i as u16;
                    if let Some(cell) = self.cell_mut((cx, y)) {
                        cell.reset();
                    }
                }
                index -= width;
                x_offset += width;
            }
        }
        (x_offset as u16, y)
    }

    fn set_string_anchored(
        &mut self,
        x: u16,
        y: u16,
        truncate_start: bool,
        truncate_end: bool,
        string: &str,
        width: usize,
        style: impl Fn(usize) -> Style,
    ) -> (u16, u16) {
        if !self.in_bounds(x, y) || width == 0 {
            return (x, y);
        }

        let mut index = ((y - self.area.y) as usize) * (self.area.width as usize)
            + ((x - self.area.x) as usize);
        let mut rendered_width = 0;
        let mut graphemes = string.grapheme_indices(true);

        if truncate_start {
            for _ in 0..graphemes.next().map(|(_, g)| g.width()).unwrap_or_default() {
                let cx = self.area.x + (index % self.area.width as usize) as u16;
                self[(cx, y)].set_symbol("…");
                index += 1;
                rendered_width += 1;
            }
        }

        for (byte_offset, s) in graphemes {
            let grapheme_width = s.width();
            if truncate_end && rendered_width + grapheme_width >= width {
                break;
            }
            if grapheme_width == 0 {
                continue;
            }

            let cx = self.area.x + (index % self.area.width as usize) as u16;
            self[(cx, y)].set_symbol(s);
            self[(cx, y)].set_style(ratatui::style::Style::from(style(byte_offset)));

            // Reset following cells if multi-width
            for i in 1..grapheme_width {
                let ncx = cx + i as u16;
                if let Some(cell) = self.cell_mut((ncx, y)) {
                    cell.reset();
                }
            }

            index += grapheme_width;
            rendered_width += grapheme_width;
        }

        if truncate_end {
            for _ in 0..width.saturating_sub(rendered_width) {
                let cx = self.area.x + (index % self.area.width as usize) as u16;
                self[(cx, y)].set_symbol("…");
                index += 1;
            }
        }

        (x, y)
    }
}
