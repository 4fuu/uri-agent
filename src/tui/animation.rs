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
pub(super) fn wordmark(frame: usize) -> Vec<String> {
    render_mark(frame, MARK[0].len())
}

/// Reveal the wordmark from left to right during the startup splash.
pub(super) fn wordmark_reveal(frame: usize, progress: f32) -> Vec<String> {
    let columns = ((MARK[0].len() as f32) * progress.clamp(0.0, 1.0)).ceil() as usize;
    render_mark(frame, columns.max(1))
}

fn render_mark(frame: usize, visible_columns: usize) -> Vec<String> {
    MARK.iter()
        .enumerate()
        .map(|(y, row)| {
            let cells = row.as_bytes();
            let mut rendered = String::with_capacity(cells.len() * 2);
            for (x, cell) in cells.iter().enumerate() {
                let phase = (BAYER_4X4[y % 4][x % 4] + frame / 2) % 16;
                let symbol = if x >= visible_columns {
                    ' '
                } else if *cell == b'1' {
                    if phase == 0 { '▓' } else { '█' }
                } else if touches_mark(x, y) && phase < 3 {
                    '·'
                } else {
                    ' '
                };
                rendered.push(symbol);
                rendered.push(symbol);
            }
            rendered
        })
        .collect()
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

pub(super) fn activity(frame: usize, width: usize) -> String {
    const LEVELS: [char; 5] = ['·', '░', '▒', '▓', '█'];
    (0..width)
        .map(|x| {
            let wave = (x + frame) % (LEVELS.len() * 2 - 2);
            LEVELS[wave.min(LEVELS.len() * 2 - 2 - wave)]
        })
        .collect()
}

pub(super) fn progress(frame: usize, width: usize, ratio: f64) -> String {
    let filled = ratio.clamp(0.0, 1.0) * width as f64;
    activity(frame, width)
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

pub(super) fn spinner(frame: usize) -> char {
    const FRAMES: [char; 8] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧'];
    FRAMES[frame % FRAMES.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wordmark_shimmers_without_layout_jitter() {
        let first = wordmark(0);
        let later = wordmark(8);
        assert_ne!(first, later);
        assert_eq!(first.len(), later.len());
        assert!(
            first
                .iter()
                .zip(later.iter())
                .all(|(left, right)| left.chars().count() == right.chars().count())
        );
        let intro = wordmark_reveal(3, 0.4);
        assert_eq!(intro.len(), first.len());
    }

    #[test]
    fn activity_has_a_deterministic_fixed_width() {
        assert_eq!(activity(0, 18).chars().count(), 18);
        assert_eq!(activity(0, 18), activity(0, 18));
        assert_ne!(activity(0, 18), activity(1, 18));
    }

    #[test]
    fn progress_has_a_stable_width_and_tracks_the_ratio() {
        assert_eq!(progress(0, 8, 0.0), "········");
        assert_eq!(progress(0, 8, 1.0).chars().count(), 8);
        assert!(!progress(0, 8, 1.0).contains('·'));
        assert_eq!(progress(0, 8, 0.5).matches('·').count(), 4);
        assert_ne!(progress(0, 8, 0.5), progress(1, 8, 0.5));
        assert_eq!(progress(0, 8, -1.0), progress(0, 8, 0.0));
        assert_eq!(progress(0, 8, 2.0), progress(0, 8, 1.0));
    }
}
