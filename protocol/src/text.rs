//! Characters that hide text from the person it is shown to.

use unicode_properties::{GeneralCategory, UnicodeGeneralCategory};

/// Whether `c`, after `previous` (the character before it, if any), can
/// carry text a model reads but the person shown it does not see:
///
/// - a control character (category Cc);
/// - a format character (category Cf: the soft hyphen, the zero-width space
///   and joiners, the bidirectional embeddings, overrides and isolates, the
///   byte order mark and the rest);
/// - a tag character (U+E0000 to U+E007F, assigned or not);
/// - a supplementary variation selector (U+E0100 to U+E01EF);
/// - a variation selector right after another. One after a character picks
///   its glyph, as U+FE0F after an emoji does; a run of them can spell out
///   anything.
///
/// Text written for another party's model, such as a plan, a reminder or a
/// failure excerpt, refuses or drops these ([`marked`]); each caller says
/// which of tab and the line breaks it keeps. Refusing Cf also refuses the
/// zero-width joiners that some scripts and emoji sequences use.
#[must_use]
pub fn is_hidden(previous: Option<char>, c: char) -> bool {
    matches!(c.general_category(), GeneralCategory::Control | GeneralCategory::Format)
        || ('\u{E0000}'..='\u{E007F}').contains(&c)
        || ('\u{E0100}'..='\u{E01EF}').contains(&c)
        || is_variation_selector(c) && previous.is_some_and(is_variation_selector)
}

/// Each character of `text` with whether it is hidden there ([`is_hidden`]
/// after the character before it).
pub fn marked(text: &str) -> impl Iterator<Item = (char, bool)> + '_ {
    text.chars().scan(None, |previous, c| {
        let hidden = is_hidden(*previous, c);
        *previous = Some(c);
        Some((c, hidden))
    })
}

fn is_variation_selector(c: char) -> bool {
    matches!(c, '\u{180B}'..='\u{180D}' | '\u{180F}' | '\u{FE00}'..='\u{FE0F}' | '\u{E0100}'..='\u{E01EF}')
}

#[cfg(test)]
mod tests {
    use super::marked;

    #[test]
    fn controls_format_characters_tags_and_selector_runs_are_hidden_and_visible_text_is_not() {
        // Each text, and the positions of the characters hidden in it.
        for (text, hidden) in [
            ("a", &[][..]),
            ("é", &[]),
            ("🦀", &[]),
            ("\u{a0}", &[]),                    // no-break space: blank, but seen as a space
            ("\u{2028}", &[]),                  // line separator: a visible line break
            ("\u{e0080}", &[]),                 // past the tag block
            ("\t", &[0]),                       // Cc: each caller decides whether to keep it
            ("\u{1b}", &[0]),                   // escape
            ("\u{85}", &[0]),                   // next line
            ("\u{ad}", &[0]),                   // soft hyphen
            ("\u{200b}", &[0]),                 // zero-width space
            ("\u{200d}", &[0]),                 // zero-width joiner
            ("\u{202e}", &[0]),                 // right-to-left override
            ("\u{2066}", &[0]),                 // left-to-right isolate
            ("\u{feff}", &[0]),                 // byte order mark
            ("\u{1d173}", &[0]),                // musical symbol begin beam
            ("\u{e0000}", &[0]),                // unassigned, in the tag block
            ("\u{e0041}", &[0]),                // tag Latin capital A
            ("\u{e007f}", &[0]),                // cancel tag
            ("\u{2764}\u{fe0f}", &[]),          // an emoji and the selector that asks for its colour
            ("a\u{fe00}b\u{fe01}", &[]),        // one selector after each character
            ("\u{2764}\u{fe0f}\u{fe0f}", &[2]), // a selector right after another
            ("a\u{fe00}\u{fe01}\u{fe02}", &[2, 3]),
            ("\u{1820}\u{180b}\u{180c}", &[2]), // Mongolian free variation selectors
            ("a\u{e0100}", &[1]),               // a supplementary selector, even alone
            ("\u{e01ef}", &[0]),
            ("\u{e01f0}", &[]), // past the supplementary selectors
        ] {
            let found: Vec<usize> =
                marked(text).enumerate().filter(|(_, (_, hidden))| *hidden).map(|(at, _)| at).collect();
            assert_eq!(found, hidden, "{text:?}");
        }
    }
}
