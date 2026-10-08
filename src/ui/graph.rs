use ratatui::{buffer::Buffer, layout::Rect, widgets::Widget};

use super::theme::Gradient;

/// Dot bits of a braille cell's left and right columns, bottom dot first.
const LEFT: [u8; 4] = [0x40, 0x04, 0x02, 0x01];
const RIGHT: [u8; 4] = [0x80, 0x20, 0x10, 0x08];

/// A filled area chart drawn with braille dots: each cell holds two samples side by side and
/// four dots of height. The newest sample sits at the right edge.
pub struct BrailleGraph<'a> {
    pub data: &'a [f64],
    /// Value that fills the full height.
    pub max: f64,
    pub gradient: Gradient,
}

impl Widget for BrailleGraph<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.is_empty() || self.max <= 0.0 {
            return;
        }
        let rows = usize::from(area.height);
        let cols = usize::from(area.width);
        let data = visible(self.data, area.width);
        let pad = cols * 2 - data.len();

        // Height of sample `i` (counted from the left edge) in dots; 0 for the empty left part.
        let dots = |i: usize| -> usize {
            let Some(v) = i.checked_sub(pad).map(|k| data[k]) else {
                return 0;
            };
            if v <= 0.0 {
                return 0;
            }
            // Any activity shows at least one dot.
            ((v / self.max).min(1.0) * (rows * 4) as f64)
                .round()
                .max(1.0) as usize
        };

        for col in 0..cols {
            let (left, right) = (dots(col * 2), dots(col * 2 + 1));
            for row in 0..rows {
                let below = (rows - 1 - row) * 4; // dots under this cell
                let l = left.saturating_sub(below).min(4);
                let r = right.saturating_sub(below).min(4);
                if l == 0 && r == 0 {
                    continue;
                }
                // Colour by height, so a tall peak turns hot while its base stays cool.
                let t = (below + 2) as f32 / (rows * 4) as f32;
                let cell = &mut buf[(area.x + col as u16, area.y + row as u16)];
                cell.set_char(braille(l, r)).set_fg(self.gradient.at(t));
            }
        }
    }
}

/// A braille character with the bottom `left` and `right` dots of its two columns lit.
fn braille(left: usize, right: usize) -> char {
    let bits = LEFT[..left]
        .iter()
        .chain(&RIGHT[..right])
        .fold(0, |a, b| a | b);
    char::from_u32(0x2800 + u32::from(bits)).unwrap_or(' ')
}

/// The newest samples that fit `width` cells.
pub fn visible(data: &[f64], width: u16) -> &[f64] {
    &data[data.len().saturating_sub(usize::from(width) * 2)..]
}

/// Top of the y axis: the visible peak rounded up to 1, 2 or 5 times a power of ten,
/// and never below `floor` so idle noise doesn't fill the box.
pub fn axis_max(data: &[f64], width: u16, floor: f64) -> f64 {
    let peak = visible(data, width).iter().copied().fold(floor, f64::max);
    let magnitude = 10f64.powi(peak.log10().floor() as i32);
    [1.0, 2.0, 5.0, 10.0]
        .iter()
        .map(|step| step * magnitude)
        .find(|&top| top >= peak)
        .unwrap_or(10.0 * magnitude)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::theme::THEME;

    fn render(data: &[f64], max: f64, width: u16, height: u16) -> Buffer {
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        BrailleGraph {
            data,
            max,
            gradient: THEME.cool,
        }
        .render(area, &mut buf);
        buf
    }

    fn row(buf: &Buffer, y: u16) -> String {
        (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect()
    }

    #[test]
    fn braille_dot_layout() {
        assert_eq!(braille(4, 4), '⣿');
        assert_eq!(braille(0, 0), '\u{2800}', "no dots");
        assert_eq!(braille(1, 0), '⡀', "bottom-left dot");
        assert_eq!(braille(0, 1), '⢀', "bottom-right dot");
        assert_eq!(braille(2, 0), '⡄');
    }

    #[test]
    fn draws_two_samples_per_cell() {
        // One row is four dots tall; max 4 means a value of 4 fills it.
        let buf = render(&[4.0, 0.0, 2.0, 4.0], 4.0, 2, 1);
        assert_eq!(row(&buf, 0), "⡇⣼");
    }

    #[test]
    fn newest_sample_is_at_the_right_edge() {
        let buf = render(&[4.0], 4.0, 3, 1);
        assert_eq!(row(&buf, 0), "  ⢸");
    }

    #[test]
    fn keeps_only_the_newest_samples_that_fit() {
        let data = [4.0, 4.0, 0.0, 0.0];
        assert_eq!(
            row(&render(&data, 4.0, 1, 1), 0),
            " ",
            "only the two zeros fit"
        );
    }

    #[test]
    fn taller_graphs_fill_from_the_bottom() {
        // Two rows = eight dots; 4 of 8 fills only the lower row.
        let buf = render(&[4.0, 4.0], 8.0, 1, 2);
        assert_eq!(row(&buf, 0), " ");
        assert_eq!(row(&buf, 1), "⣿");
    }

    #[test]
    fn any_activity_shows_one_dot() {
        let buf = render(&[0.001, 0.001], 1000.0, 1, 1);
        assert_eq!(row(&buf, 0), "⣀");
    }

    #[test]
    fn high_cells_are_hotter_than_low_ones() {
        let buf = render(&[8.0, 8.0], 8.0, 1, 2);
        assert_ne!(buf[(0, 0)].fg, buf[(0, 1)].fg);
    }

    #[test]
    fn axis_rounds_up_to_one_two_five() {
        assert_eq!(axis_max(&[1234.0], 10, 10.0), 2000.0);
        assert_eq!(axis_max(&[1000.0], 10, 10.0), 1000.0);
        assert_eq!(axis_max(&[3.0], 10, 10.0), 10.0, "floor wins");
        assert_eq!(axis_max(&[11.0], 10, 10.0), 20.0);
        assert_eq!(axis_max(&[330.0], 10, 10.0), 500.0);
        assert_eq!(axis_max(&[], 10, 4.0), 5.0);
    }

    #[test]
    fn axis_ignores_samples_that_scrolled_off() {
        // Width 1 shows two samples; the 9000 is out of view.
        assert_eq!(axis_max(&[9000.0, 1.0, 2.0], 1, 1.0), 2.0);
    }

    #[test]
    fn empty_or_degenerate_areas_do_nothing() {
        render(&[1.0], 1.0, 0, 0);
        render(&[1.0], 0.0, 4, 2);
        render(&[], 1.0, 4, 2);
    }
}
