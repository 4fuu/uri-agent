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
    MARK.iter()
        .enumerate()
        .map(|(y, row)| {
            let cells = row.as_bytes();
            let mut rendered = String::with_capacity(cells.len() * 2);
            for (x, cell) in cells.iter().enumerate() {
                let phase = (BAYER_4X4[y % 4][x % 4] + frame / 2) % 16;
                let symbol = if *cell == b'1' {
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
    }

    #[test]
    fn activity_has_a_deterministic_fixed_width() {
        assert_eq!(activity(0, 18).chars().count(), 18);
        assert_eq!(activity(0, 18), activity(0, 18));
        assert_ne!(activity(0, 18), activity(1, 18));
    }
}
