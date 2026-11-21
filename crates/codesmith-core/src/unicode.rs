//! Unicode characters that draw nothing but change how text reads, for the
//! places that strip text before showing it: bidi controls (an embedded
//! U+202E makes `ok.txt` read as `txt.ko`), zero-width characters and the
//! other format characters text from a command or the model can hide in.

/// Whether `c` is a format character stripped from text before it is shown:
/// the Arabic letter mark, the Mongolian vowel separator, zero-width space,
/// non-joiner and joiner, the left-to-right and right-to-left marks, the
/// line and paragraph separators, the bidi embeddings, overrides and pops
/// (U+202A–U+202E), the word joiner and invisible operators
/// (U+2060–U+2064), the bidi isolates (U+2066–U+2069) and the deprecated
/// format controls after them (U+206A–U+206F), the byte order mark, the
/// interlinear annotation characters and the tag characters.
pub fn format_char(c: char) -> bool {
    matches!(
        c as u32,
        0x061C
            | 0x180E
            | 0x200B..=0x200F
            | 0x2028..=0x202E
            | 0x2060..=0x2064
            | 0x2066..=0x206F
            | 0xFEFF
            | 0xFFF9..=0xFFFB
            | 0xE0000..=0xE007F
    )
}

#[cfg(test)]
mod tests {
    use super::format_char;

    #[test]
    fn bidi_and_zero_width_characters_are_format_characters() {
        for c in [
            '\u{202a}',
            '\u{202b}',
            '\u{202c}',
            '\u{202d}',
            '\u{202e}',
            '\u{2066}',
            '\u{2067}',
            '\u{2068}',
            '\u{2069}',
            '\u{200b}',
            '\u{200c}',
            '\u{200d}',
            '\u{200e}',
            '\u{200f}',
            '\u{2060}',
            '\u{feff}',
            '\u{061c}',
            '\u{2028}',
            '\u{2029}',
            '\u{e0041}',
        ] {
            assert!(format_char(c), "{:x}", c as u32);
        }
        for c in [
            'a',
            ' ',
            'é',
            'ホ',
            '\u{1f600}',
            '\u{2065}',
            '\u{2070}',
            '\u{2027}',
            '\u{fe0f}',
            '\u{00a0}',
            '\u{3000}',
        ] {
            assert!(!format_char(c), "{:x}", c as u32);
        }
    }
}
