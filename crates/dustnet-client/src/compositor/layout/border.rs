use super::cell::{CellBuffer, CellStyle};
use crate::parser::ast::BorderStyle;

/// The characters that make up a box border.
#[derive(Debug, Clone, Copy)]
pub struct BorderChars {
    pub top_left: char,
    pub top_right: char,
    pub bottom_left: char,
    pub bottom_right: char,
    pub horizontal: char,
    pub vertical: char,
}

impl BorderChars {
    /// Get the border characters for a given border style.
    pub fn for_style(style: BorderStyle) -> Option<Self> {
        match style {
            BorderStyle::None => None,
            BorderStyle::Single => Some(BorderChars {
                top_left: '┌',
                top_right: '┐',
                bottom_left: '└',
                bottom_right: '┘',
                horizontal: '─',
                vertical: '│',
            }),
            BorderStyle::Double => Some(BorderChars {
                top_left: '╔',
                top_right: '╗',
                bottom_left: '╚',
                bottom_right: '╝',
                horizontal: '═',
                vertical: '║',
            }),
            BorderStyle::Rounded => Some(BorderChars {
                top_left: '╭',
                top_right: '╮',
                bottom_left: '╰',
                bottom_right: '╯',
                horizontal: '─',
                vertical: '│',
            }),
            BorderStyle::Heavy => Some(BorderChars {
                top_left: '┏',
                top_right: '┓',
                bottom_left: '┗',
                bottom_right: '┛',
                horizontal: '━',
                vertical: '┃',
            }),
            BorderStyle::Ascii => Some(BorderChars {
                top_left: '+',
                top_right: '+',
                bottom_left: '+',
                bottom_right: '+',
                horizontal: '-',
                vertical: '|',
            }),
        }
    }
}

/// Get the character used for a horizontal rule style.
pub fn hr_char(style: crate::parser::ast::HrStyle) -> char {
    use crate::parser::ast::HrStyle;
    match style {
        HrStyle::Single => '─',
        HrStyle::Double => '═',
        HrStyle::Heavy => '━',
        HrStyle::Dash => '╌',
        HrStyle::Dot => '·',
        HrStyle::Ascii => '-',
    }
}

/// Draw a box border on the cell buffer.
///
/// Draws the border around the rectangle defined by (x, y, w, h).
/// The border occupies the outermost row/column of the rectangle.
/// An optional title is rendered into the top border.
///
/// Returns the inner rect (x, y, w, h) available for content after
/// accounting for the border.
#[allow(clippy::too_many_arguments)]
pub fn draw_border(
    buf: &mut CellBuffer,
    x: u16,
    y: u16,
    w: u16,
    h: u16,
    style: BorderStyle,
    title: Option<&str>,
    cell_style: &CellStyle,
) -> (u16, u16, u16, u16) {
    draw_border_with_joints(
        buf, x, y, w, h, style, title, cell_style, None, None, None, None,
    )
}

/// Draw a box border with optional connector junctions. Junction offsets are
/// local to the box: top/bottom count columns from the left edge, while
/// left/right count rows from the top edge.
#[allow(clippy::too_many_arguments)]
pub fn draw_border_with_joints(
    buf: &mut CellBuffer,
    x: u16,
    y: u16,
    w: u16,
    h: u16,
    style: BorderStyle,
    title: Option<&str>,
    cell_style: &CellStyle,
    join_top: Option<u16>,
    join_bottom: Option<u16>,
    join_left: Option<u16>,
    join_right: Option<u16>,
) -> (u16, u16, u16, u16) {
    let chars = match BorderChars::for_style(style) {
        Some(c) => c,
        None => {
            // No border — full rect available for content
            return (x, y, w, h);
        }
    };

    if w < 2 || h < 2 {
        // Too small to draw a border
        return (x, y, 0, 0);
    }

    let right = x + w - 1;
    let bottom = y + h - 1;

    // Corners
    buf.put_char(x, y, chars.top_left, cell_style);
    buf.put_char(right, y, chars.top_right, cell_style);
    buf.put_char(x, bottom, chars.bottom_left, cell_style);
    buf.put_char(right, bottom, chars.bottom_right, cell_style);

    // Top edge
    for col in (x + 1)..right {
        buf.put_char(col, y, chars.horizontal, cell_style);
    }

    // Bottom edge
    for col in (x + 1)..right {
        buf.put_char(col, bottom, chars.horizontal, cell_style);
    }

    // Left and right edges
    for row in (y + 1)..bottom {
        buf.put_char(x, row, chars.vertical, cell_style);
        buf.put_char(right, row, chars.vertical, cell_style);
    }

    // Title (rendered into top border)
    if let Some(title) = title
        && w > 4
    {
        let max_title_len = (w - 4) as usize; // leave room for corners + space
        let display_title = if title.len() > max_title_len {
            // Truncate with ellipsis
            let mut truncated: String = title.chars().take(max_title_len - 1).collect();
            truncated.push('…');
            truncated
        } else {
            title.to_string()
        };

        // Write title after a space: ┌ Title ─────┐
        let title_x = x + 2;
        buf.put_char(x + 1, y, ' ', cell_style);
        buf.put_str(title_x, y, &display_title, cell_style);
        let after_title = title_x + display_title.len() as u16;
        if after_title < right {
            buf.put_char(after_title, y, ' ', cell_style);
        }
    }

    let (top_joint, bottom_joint, left_joint, right_joint) = match style {
        BorderStyle::Single | BorderStyle::Rounded => ('┴', '┬', '┤', '├'),
        BorderStyle::Double => ('╩', '╦', '╣', '╠'),
        BorderStyle::Heavy => ('┻', '┳', '┫', '┣'),
        BorderStyle::Ascii => ('+', '+', '+', '+'),
        BorderStyle::None => (' ', ' ', ' ', ' '),
    };

    if let Some(offset) = join_top.filter(|offset| *offset > 0 && *offset + 1 < w) {
        buf.put_char(x + offset, y, top_joint, cell_style);
    }
    if let Some(offset) = join_bottom.filter(|offset| *offset > 0 && *offset + 1 < w) {
        buf.put_char(x + offset, bottom, bottom_joint, cell_style);
    }
    if let Some(offset) = join_left.filter(|offset| *offset > 0 && *offset + 1 < h) {
        buf.put_char(x, y + offset, left_joint, cell_style);
    }
    if let Some(offset) = join_right.filter(|offset| *offset > 0 && *offset + 1 < h) {
        buf.put_char(right, y + offset, right_joint, cell_style);
    }

    // Inner rect: inside the border
    (x + 1, y + 1, w.saturating_sub(2), h.saturating_sub(2))
}

/// Draw a horizontal rule across the full width at the given row.
pub fn draw_hr(
    buf: &mut CellBuffer,
    y: u16,
    x_start: u16,
    width: u16,
    hr_style: crate::parser::ast::HrStyle,
    cell_style: &CellStyle,
) {
    let ch = hr_char(hr_style);
    for col in x_start..(x_start + width).min(buf.width) {
        buf.put_char(col, y, ch, cell_style);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_style() -> CellStyle {
        CellStyle::default()
    }

    #[test]
    fn single_border_corners() {
        let mut buf = CellBuffer::new(10, 5);
        draw_border(
            &mut buf,
            0,
            0,
            10,
            5,
            BorderStyle::Single,
            None,
            &default_style(),
        );

        assert_eq!(buf.get(0, 0).unwrap().ch, '┌');
        assert_eq!(buf.get(9, 0).unwrap().ch, '┐');
        assert_eq!(buf.get(0, 4).unwrap().ch, '└');
        assert_eq!(buf.get(9, 4).unwrap().ch, '┘');
    }

    #[test]
    fn single_border_edges() {
        let mut buf = CellBuffer::new(10, 5);
        draw_border(
            &mut buf,
            0,
            0,
            10,
            5,
            BorderStyle::Single,
            None,
            &default_style(),
        );

        // Top edge
        assert_eq!(buf.get(1, 0).unwrap().ch, '─');
        assert_eq!(buf.get(8, 0).unwrap().ch, '─');
        // Left edge
        assert_eq!(buf.get(0, 1).unwrap().ch, '│');
        assert_eq!(buf.get(0, 3).unwrap().ch, '│');
        // Right edge
        assert_eq!(buf.get(9, 1).unwrap().ch, '│');
        // Bottom edge
        assert_eq!(buf.get(1, 4).unwrap().ch, '─');
    }

    #[test]
    fn single_border_inner_rect() {
        let mut buf = CellBuffer::new(20, 10);
        let (ix, iy, iw, ih) = draw_border(
            &mut buf,
            2,
            3,
            10,
            6,
            BorderStyle::Single,
            None,
            &default_style(),
        );
        assert_eq!((ix, iy, iw, ih), (3, 4, 8, 4));
    }

    #[test]
    fn double_border() {
        let mut buf = CellBuffer::new(10, 5);
        draw_border(
            &mut buf,
            0,
            0,
            10,
            5,
            BorderStyle::Double,
            None,
            &default_style(),
        );
        assert_eq!(buf.get(0, 0).unwrap().ch, '╔');
        assert_eq!(buf.get(9, 0).unwrap().ch, '╗');
        assert_eq!(buf.get(1, 0).unwrap().ch, '═');
        assert_eq!(buf.get(0, 1).unwrap().ch, '║');
    }

    #[test]
    fn rounded_border() {
        let mut buf = CellBuffer::new(10, 5);
        draw_border(
            &mut buf,
            0,
            0,
            10,
            5,
            BorderStyle::Rounded,
            None,
            &default_style(),
        );
        assert_eq!(buf.get(0, 0).unwrap().ch, '╭');
        assert_eq!(buf.get(9, 0).unwrap().ch, '╮');
        assert_eq!(buf.get(0, 4).unwrap().ch, '╰');
        assert_eq!(buf.get(9, 4).unwrap().ch, '╯');
    }

    #[test]
    fn heavy_border() {
        let mut buf = CellBuffer::new(10, 5);
        draw_border(
            &mut buf,
            0,
            0,
            10,
            5,
            BorderStyle::Heavy,
            None,
            &default_style(),
        );
        assert_eq!(buf.get(0, 0).unwrap().ch, '┏');
        assert_eq!(buf.get(1, 0).unwrap().ch, '━');
        assert_eq!(buf.get(0, 1).unwrap().ch, '┃');
    }

    #[test]
    fn ascii_border() {
        let mut buf = CellBuffer::new(10, 5);
        draw_border(
            &mut buf,
            0,
            0,
            10,
            5,
            BorderStyle::Ascii,
            None,
            &default_style(),
        );
        assert_eq!(buf.get(0, 0).unwrap().ch, '+');
        assert_eq!(buf.get(1, 0).unwrap().ch, '-');
        assert_eq!(buf.get(0, 1).unwrap().ch, '|');
    }

    #[test]
    fn no_border_returns_full_rect() {
        let mut buf = CellBuffer::new(20, 10);
        let (ix, iy, iw, ih) = draw_border(
            &mut buf,
            5,
            3,
            10,
            6,
            BorderStyle::None,
            None,
            &default_style(),
        );
        assert_eq!((ix, iy, iw, ih), (5, 3, 10, 6));
        // Nothing should be drawn
        assert_eq!(buf.get(5, 3).unwrap().ch, ' ');
    }

    #[test]
    fn border_with_title() {
        let mut buf = CellBuffer::new(20, 5);
        draw_border(
            &mut buf,
            0,
            0,
            20,
            5,
            BorderStyle::Single,
            Some("Status"),
            &default_style(),
        );

        // Title should appear in top border: ┌ Status ──────────┐
        assert_eq!(buf.get(0, 0).unwrap().ch, '┌');
        assert_eq!(buf.get(1, 0).unwrap().ch, ' ');
        assert_eq!(buf.get(2, 0).unwrap().ch, 'S');
        assert_eq!(buf.get(3, 0).unwrap().ch, 't');
        assert_eq!(buf.get(4, 0).unwrap().ch, 'a');
        assert_eq!(buf.get(5, 0).unwrap().ch, 't');
        assert_eq!(buf.get(6, 0).unwrap().ch, 'u');
        assert_eq!(buf.get(7, 0).unwrap().ch, 's');
        assert_eq!(buf.get(8, 0).unwrap().ch, ' ');
    }

    #[test]
    fn border_with_long_title_truncates() {
        let mut buf = CellBuffer::new(12, 3);
        draw_border(
            &mut buf,
            0,
            0,
            12,
            3,
            BorderStyle::Single,
            Some("A Very Long Title"),
            &default_style(),
        );

        // Max title len = 12 - 4 = 8. Title should be truncated with '…'
        // Position 2 starts the title
        assert_eq!(buf.get(2, 0).unwrap().ch, 'A');
        // The title should be truncated somewhere
        let top_row: String = (0..12).map(|x| buf.get(x, 0).unwrap().ch).collect();
        assert!(top_row.contains('…'));
    }

    #[test]
    fn too_small_for_border() {
        let mut buf = CellBuffer::new(5, 5);
        let (_, _, iw, ih) = draw_border(
            &mut buf,
            0,
            0,
            1,
            1,
            BorderStyle::Single,
            None,
            &default_style(),
        );
        // Too small — inner rect should be 0x0
        assert_eq!(iw, 0);
        assert_eq!(ih, 0);
    }

    #[test]
    fn border_offset_position() {
        let mut buf = CellBuffer::new(20, 10);
        draw_border(
            &mut buf,
            5,
            2,
            8,
            4,
            BorderStyle::Single,
            None,
            &default_style(),
        );

        assert_eq!(buf.get(5, 2).unwrap().ch, '┌');
        assert_eq!(buf.get(12, 2).unwrap().ch, '┐');
        assert_eq!(buf.get(5, 5).unwrap().ch, '└');
        assert_eq!(buf.get(12, 5).unwrap().ch, '┘');
        // Area outside the box should be untouched
        assert_eq!(buf.get(4, 2).unwrap().ch, ' ');
    }

    // ─── HR Tests ────────────────────────────────────────────

    #[test]
    fn hr_single() {
        let mut buf = CellBuffer::new(10, 1);
        draw_hr(
            &mut buf,
            0,
            0,
            10,
            crate::parser::ast::HrStyle::Single,
            &default_style(),
        );
        assert_eq!(buf.get(0, 0).unwrap().ch, '─');
        assert_eq!(buf.get(9, 0).unwrap().ch, '─');
    }

    #[test]
    fn hr_dash() {
        let mut buf = CellBuffer::new(10, 1);
        draw_hr(
            &mut buf,
            0,
            0,
            10,
            crate::parser::ast::HrStyle::Dash,
            &default_style(),
        );
        assert_eq!(buf.get(0, 0).unwrap().ch, '╌');
    }

    #[test]
    fn hr_dot() {
        let mut buf = CellBuffer::new(10, 1);
        draw_hr(
            &mut buf,
            0,
            0,
            10,
            crate::parser::ast::HrStyle::Dot,
            &default_style(),
        );
        assert_eq!(buf.get(0, 0).unwrap().ch, '·');
    }

    #[test]
    fn hr_ascii() {
        let mut buf = CellBuffer::new(10, 1);
        draw_hr(
            &mut buf,
            0,
            0,
            10,
            crate::parser::ast::HrStyle::Ascii,
            &default_style(),
        );
        assert_eq!(buf.get(0, 0).unwrap().ch, '-');
    }

    #[test]
    fn hr_partial_width() {
        let mut buf = CellBuffer::new(20, 1);
        draw_hr(
            &mut buf,
            0,
            5,
            8,
            crate::parser::ast::HrStyle::Single,
            &default_style(),
        );
        assert_eq!(buf.get(4, 0).unwrap().ch, ' '); // before
        assert_eq!(buf.get(5, 0).unwrap().ch, '─'); // start
        assert_eq!(buf.get(12, 0).unwrap().ch, '─'); // end
        assert_eq!(buf.get(13, 0).unwrap().ch, ' '); // after
    }

    #[test]
    fn hr_chars_all_styles() {
        use crate::parser::ast::HrStyle;
        assert_eq!(hr_char(HrStyle::Single), '─');
        assert_eq!(hr_char(HrStyle::Double), '═');
        assert_eq!(hr_char(HrStyle::Heavy), '━');
        assert_eq!(hr_char(HrStyle::Dash), '╌');
        assert_eq!(hr_char(HrStyle::Dot), '·');
        assert_eq!(hr_char(HrStyle::Ascii), '-');
    }

    #[test]
    fn single_border_connector_joints_replace_edge_cells() {
        let mut buf = CellBuffer::new(12, 7);
        draw_border_with_joints(
            &mut buf,
            0,
            0,
            12,
            7,
            BorderStyle::Single,
            None,
            &default_style(),
            Some(5),
            Some(6),
            Some(3),
            Some(4),
        );

        assert_eq!(buf.get(5, 0).unwrap().ch, '┴');
        assert_eq!(buf.get(6, 6).unwrap().ch, '┬');
        assert_eq!(buf.get(0, 3).unwrap().ch, '┤');
        assert_eq!(buf.get(11, 4).unwrap().ch, '├');
    }
}
