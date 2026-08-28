use unicode_general_category::{GeneralCategory, get_general_category};

pub(crate) fn visible_units(text: &str) -> usize {
    count_visible_units(text, false).0
}

pub(crate) fn count_visible_units(text: &str, starts_in_word: bool) -> (usize, bool) {
    let mut count = 0usize;
    let mut in_word = starts_in_word;
    for character in text.chars() {
        if character.is_whitespace() {
            in_word = false;
        } else if is_cjk_visible_unit(character) {
            count = count.saturating_add(1);
            in_word = false;
        } else if character == '_' || character.is_alphanumeric() {
            if !in_word {
                count = count.saturating_add(1);
            }
            in_word = true;
        } else if !matches!(
            get_general_category(character),
            GeneralCategory::Control
                | GeneralCategory::Format
                | GeneralCategory::Surrogate
                | GeneralCategory::PrivateUse
                | GeneralCategory::Unassigned
                | GeneralCategory::NonspacingMark
                | GeneralCategory::SpacingMark
                | GeneralCategory::EnclosingMark
        ) {
            count = count.saturating_add(1);
            in_word = false;
        }
    }
    (count, in_word)
}

fn is_cjk_visible_unit(character: char) -> bool {
    matches!(
        character as u32,
        0x3400..=0x4dbf
            | 0x4e00..=0x9fff
            | 0xf900..=0xfaff
            | 0x20000..=0x2ebef
            | 0x3040..=0x30ff
            | 0x31f0..=0x31ff
            | 0xac00..=0xd7af
            | 0x1100..=0x11ff
    )
}
