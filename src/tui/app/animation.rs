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

/// A stable pixel wordmark with a small ordered-dither shimmer. The mark does
/// not change dimensions between frames, so it can animate without moving the
/// rest of the layout.
pub(super) fn wordmark(phase: f64) -> Vec<String> {
    render_mark(phase, MARK[0].len())
}

/// Reveal the wordmark from left to right during the startup splash.
pub(super) fn wordmark_reveal(phase: f64, progress: f32) -> Vec<String> {
    let columns = ((MARK[0].len() as f32) * progress.clamp(0.0, 1.0)).ceil() as usize;
    render_mark(phase, columns.max(1))
}

fn render_mark(phase: f64, visible_columns: usize) -> Vec<String> {
    let frame = phase.floor().max(0.0) as usize;
    let fraction = phase.fract().max(0.0);
    MARK.iter()
        .enumerate()
        .map(|(y, row)| {
            let cells = row.as_bytes();
            let mut rendered = String::with_capacity(cells.len() * 2);
            for x in 0..cells.len() {
                let current = mark_symbol(frame, x, y, visible_columns);
                let next = mark_symbol(frame.wrapping_add(1), x, y, visible_columns);
                let threshold = (BAYER_4X4[y % 4][x % 4] + 1) as f64 / 17.0;
                let symbol = if fraction >= threshold { next } else { current };
                rendered.push(symbol);
                rendered.push(symbol);
            }
            rendered
        })
        .collect()
}

fn mark_symbol(frame: usize, x: usize, y: usize, visible_columns: usize) -> char {
    let phase = (BAYER_4X4[y % 4][x % 4] + frame / 2) % 16;
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
            // Stagger cell transitions across the legacy interval. Integer
            // phases retain the exact old frame, while fractional phases make
            // intermediate movement visible at the presentation cadence.
            let threshold = ((x * 5) % width + 1) as f64 / (width + 1) as f64;
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
        let first = wordmark(0.0);
        let later = wordmark(8.0);
        assert_ne!(first, later);
        assert_eq!(first.len(), later.len());
        assert!(
            first
                .iter()
                .zip(later.iter())
                .all(|(left, right)| left.chars().count() == right.chars().count())
        );
        let intro = wordmark_reveal(3.0, 0.4);
        assert_eq!(intro.len(), first.len());
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

    #[test]
    fn wordmark_interpolates_but_legacy_samples_and_spinner_timing_remain_compatible() {
        let between = wordmark(1.5);
        assert_ne!(between, wordmark(1.0));
        assert_ne!(between, wordmark(2.0));

        assert_eq!(spinner(0.0), spinner(0.99));
        assert_ne!(spinner(0.99), spinner(1.0));
        assert_eq!(spinner(0.0), spinner(8.0));
    }
}
