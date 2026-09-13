//! Sentence-terminal detection for transcribed utterances.
//!
//! The WS session can hold a finalized utterance's transcript instead of
//! dispatching it to the LLM immediately: when the transcript does not end
//! with sentence-terminal punctuation, the utterance is probably a mid-sentence
//! pause (VAD fired on a breath, an "uh", a hesitation) rather than the end of
//! a request. [`ends_sentence`] decides terminal-ness; [`join_transcripts`]
//! concatenates held fragments with a single space.

/// True when `text` ends with sentence-terminal punctuation (`.`, `!`, `?`, `…`).
///
/// Scans backward over trailing whitespace and closing quotes/brackets so
/// `Why not?`, `He said "stop."` and `(really!)` all count as terminal.
/// Empty or punctuation-free text is not terminal — a fragment like
/// `can you look up the weather` must be held, not dispatched.
///
/// Deliberately no abbreviation list: `Mr.` ends a "sentence" as far as the
/// gate is concerned, which is the safer failure mode (dispatching a complete
/// sounding fragment beats holding a finished command).
pub fn ends_sentence(text: &str) -> bool {
    for c in text.chars().rev() {
        if c.is_whitespace() || matches!(c, '"' | '”' | '’' | ')' | ']' | '»') {
            continue;
        }
        return matches!(c, '.' | '!' | '?' | '…');
    }
    false
}

/// Concatenate a held transcript fragment with the next one: single-space
/// join after trimming. Either side empty → the other side unchanged.
pub fn join_transcripts(held: &str, next: &str) -> String {
    let held = held.trim();
    let next = next.trim();
    match (held.is_empty(), next.is_empty()) {
        (true, _) => next.to_string(),
        (_, true) => held.to_string(),
        (false, false) => format!("{held} {next}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_punctuation_is_detected() {
        assert!(ends_sentence("what time is it?"));
        assert!(ends_sentence("turn off the lights."));
        assert!(ends_sentence("that is amazing!"));
        assert!(ends_sentence("wait for it…"));
        assert!(ends_sentence("wait for it...")); // three ASCII dots
    }

    #[test]
    fn trailing_whitespace_and_quotes_are_skipped() {
        assert!(ends_sentence("what time is it?  "));
        assert!(ends_sentence("he said \"stop.\""));
        assert!(ends_sentence("he said “stop.”"));
        assert!(ends_sentence("(really!)"));
        assert!(ends_sentence("[done?]"));
    }

    #[test]
    fn nonterminal_fragments_are_held() {
        assert!(!ends_sentence("can you look up the weather"));
        assert!(!ends_sentence("can you look up uh"));
        assert!(!ends_sentence("hello   "));
        assert!(!ends_sentence(""));
        assert!(!ends_sentence("   "));
    }

    #[test]
    fn decimal_point_does_not_end_a_sentence() {
        // A number like "72.5" ends in a digit, not the dot — nonterminal.
        assert!(!ends_sentence("set it to 72.5"));
    }

    #[test]
    fn join_transcripts_single_space_join() {
        assert_eq!(
            join_transcripts("can you look up uh", "the weather for tomorrow?"),
            "can you look up uh the weather for tomorrow?"
        );
    }

    #[test]
    fn join_transcripts_trims_and_handles_empty_sides() {
        assert_eq!(join_transcripts("", "hello there"), "hello there");
        assert_eq!(join_transcripts("hello there", ""), "hello there");
        assert_eq!(join_transcripts("  spaced  ", " out "), "spaced out");
        assert_eq!(join_transcripts("", ""), "");
    }
}
