const MARK: [&str; 7] = [
    "10001011110011111",
    "10001010001000100",
    "10001010001000100",
    "10001011110000100",
    "10001010100000100",
    "10001010010000100",
    "01110010001011111",
];

const BAYER_4X4: [[usize; 4]; 4] = [[0, 8, 2, 10], [12, 4, 14, 6], [3, 11, 1, 9], [15, 7, 13, 5]];

/// Wordmark for brand boxes too narrow for the bitmap. One cell per pixel
/// already needs every mark column, so a smaller box draws the plain name
/// instead of clipping the pixel mark.
const MARK_NAME: &str = "URI";

/// A stable pixel wordmark with a small ordered-dither shimmer. The mark does
/// not change dimensions between frames, so it can animate without moving the
/// rest of the layout. `width` is the brand box the mark has to fit inside:
/// two cells per pixel while the doubled mark fits, one cell per pixel down to
/// the bitmap width, and the plain name below that.
pub(super) fn wordmark(phase: f64, width: usize) -> Vec<String> {
    match cells_per_pixel(width) {
        Some(cells) => render_mark(phase, MARK[0].len(), cells),
        None => vec![MARK_NAME.to_string()],
    }
}

/// Reveal the wordmark from left to right during the startup splash.
pub(super) fn wordmark_reveal(phase: f64, progress: f32, width: usize) -> Vec<String> {
    let Some(cells) = cells_per_pixel(width) else {
        return vec![MARK_NAME.to_string()];
    };
    let columns = ((MARK[0].len() as f32) * progress.clamp(0.0, 1.0)).ceil() as usize;
    render_mark(phase, columns.max(1), cells)
}

/// Cells spent on one mark pixel inside a brand box `width` columns wide. Two
/// cells keep the strokes solid while the box fits the doubled mark and one
/// cell is the narrowest faithful drawing; a box narrower than the bitmap has
/// no drawing that shows every column.
fn cells_per_pixel(width: usize) -> Option<usize> {
    match width / MARK[0].len() {
        0 => None,
        1 => Some(1),
        _ => Some(2),
    }
}

fn render_mark(phase: f64, visible_columns: usize, cells: usize) -> Vec<String> {
    // The original shimmer changed every two 90 ms animation samples. Move
    // continuously between those distinct states so presentation frames do
    // not sit still for one interval and then change in a correlated burst.
    let shimmer_phase = phase.max(0.0) / 2.0;
    let frame = shimmer_phase.floor() as usize;
    let fraction = shimmer_phase.fract();
    MARK.iter()
        .enumerate()
        .map(|(y, row)| {
            let pixels = row.as_bytes();
            let mut rendered = String::with_capacity(pixels.len() * cells);
            for x in 0..pixels.len() {
                let current = mark_symbol(frame, x, y, visible_columns);
                let next = mark_symbol(frame.wrapping_add(1), x, y, visible_columns);
                // Do not reuse the shimmer's Bayer class as its transition
                // time: that makes all cells in one class jump together.
                let threshold = ((x * 6 + y * 3 + 3) % 17 + 1) as f64 / 18.0;
                let symbol = if fraction >= threshold { next } else { current };
                for _ in 0..cells {
                    rendered.push(symbol);
                }
            }
            rendered
        })
        .collect()
}

fn mark_symbol(frame: usize, x: usize, y: usize, visible_columns: usize) -> char {
    let phase = (BAYER_4X4[y % 4][x % 4] + frame) % 16;
    if x >= visible_columns {
        ' '
    } else if MARK[y].as_bytes()[x] == b'1' {
        if phase == 0 { '▓' } else { '█' }
    } else if touches_mark(x, y) && phase < 3 {
        '·'
    } else {
        ' '
    }
}

fn touches_mark(x: usize, y: usize) -> bool {
    let occupied = |x: isize, y: isize| {
        x >= 0
            && y >= 0
            && MARK
                .get(y as usize)
                .and_then(|row| row.as_bytes().get(x as usize))
                == Some(&b'1')
    };
    occupied(x as isize - 1, y as isize)
        || occupied(x as isize + 1, y as isize)
        || occupied(x as isize, y as isize - 1)
        || occupied(x as isize, y as isize + 1)
}

fn legacy_activity(frame: usize, width: usize) -> String {
    const LEVELS: [char; 5] = ['·', '░', '▒', '▓', '█'];
    (0..width)
        .map(|x| {
            let wave = (x + frame) % (LEVELS.len() * 2 - 2);
            LEVELS[wave.min(LEVELS.len() * 2 - 2 - wave)]
        })
        .collect()
}

pub(super) fn activity(phase: f64, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let phase = phase.max(0.0);
    let frame = phase.floor() as usize;
    let fraction = phase.fract();
    legacy_activity(frame, width)
        .chars()
        .zip(legacy_activity(frame.wrapping_add(1), width).chars())
        .enumerate()
        .map(|(x, (current, next))| {
            // Move one coherent transition front across the wave. Integer
            // phases retain the exact old frame, while fractional phases make
            // intermediate movement visible at the presentation cadence.
            let threshold = (x + 1) as f64 / (width + 1) as f64;
            if fraction >= threshold { next } else { current }
        })
        .collect()
}

pub(super) fn progress(phase: f64, width: usize, ratio: f64) -> String {
    let filled = ratio.clamp(0.0, 1.0) * width as f64;
    activity(phase, width)
        .chars()
        .enumerate()
        .map(|(x, level)| {
            let remaining = filled - x as f64;
            if remaining <= 0.0 {
                '·'
            } else if remaining < 1.0 / 3.0 {
                '░'
            } else if remaining < 2.0 / 3.0 {
                '▒'
            } else if remaining < 1.0 {
                '▓'
            } else if level == '·' {
                '░'
            } else {
                level
            }
        })
        .collect()
}

/// Spinner glyphs are intentionally discrete: each glyph retains its legacy
/// 90 ms residence time while the surrounding interpolated animation updates.
pub(super) fn spinner(phase: f64) -> char {
    const FRAMES: [char; 8] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧'];
    FRAMES[phase.max(0.0).floor() as usize % FRAMES.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wordmark_shimmers_without_layout_jitter() {
        let first = wordmark(0.0, 80);
        let later = wordmark(8.0, 80);
        assert_ne!(first, later);
        assert_eq!(first.len(), later.len());
        assert!(
            first
                .iter()
                .zip(later.iter())
                .all(|(left, right)| left.chars().count() == right.chars().count())
        );
        let intro = wordmark_reveal(3.0, 0.4, 80);
        assert_eq!(intro.len(), first.len());
    }

    #[test]
    fn wordmark_scales_down_instead_of_being_clipped() {
        let columns = MARK[0].len();
        let roomy = wordmark(0.0, columns * 2);
        assert!(roomy.iter().all(|line| line.chars().count() == columns * 2));
        let narrow = wordmark(0.0, columns);
        assert!(narrow.iter().all(|line| line.chars().count() == columns));
        // No pixel drawing fits a box narrower than the bitmap, so the brand
        // falls back to the plain name rather than losing the last letter.
        assert_eq!(wordmark(0.0, columns - 1), vec![MARK_NAME.to_string()]);
        assert_eq!(wordmark(0.0, 0), vec![MARK_NAME.to_string()]);
    }

    #[test]
    fn reveal_stays_inside_the_brand_box_width() {
        let columns = MARK[0].len();
        for width in [1usize, columns / 2, columns - 1, columns, columns + 5, 200] {
            let revealed = wordmark_reveal(3.0, 0.4, width);
            if width >= columns {
                assert_eq!(revealed.len(), MARK.len());
                assert!(revealed.iter().all(|line| line.chars().count() <= width));
            } else {
                assert_eq!(revealed, vec![MARK_NAME.to_string()]);
            }
        }
    }

    #[test]
    fn activity_has_a_deterministic_fixed_width() {
        assert_eq!(activity(0.0, 18).chars().count(), 18);
        assert_eq!(activity(0.0, 18), activity(0.0, 18));
        assert_ne!(activity(0.0, 18), activity(1.0, 18));
        assert_ne!(activity(0.2, 18), activity(0.0, 18));
        assert_ne!(activity(0.2, 18), activity(1.0, 18));
        assert_eq!(activity(8.0, 18), activity(0.0, 18));
    }

    #[test]
    fn progress_has_a_stable_width_and_tracks_the_ratio() {
        assert_eq!(progress(0.0, 8, 0.0), "········");
        assert_eq!(progress(0.0, 8, 1.0).chars().count(), 8);
        assert!(!progress(0.0, 8, 1.0).contains('·'));
        assert_eq!(progress(0.0, 8, 0.5).matches('·').count(), 4);
        assert_ne!(progress(0.0, 8, 0.5), progress(1.0, 8, 0.5));
        assert_eq!(progress(0.0, 8, -1.0), progress(0.0, 8, 0.0));
        assert_eq!(progress(0.0, 8, 2.0), progress(0.0, 8, 1.0));
        assert_ne!(progress(0.4, 8, 1.0), progress(0.0, 8, 1.0));
    }
}
