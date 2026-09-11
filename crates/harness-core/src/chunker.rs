//! Response chunker: sentence-sized chunks for TTS + speech text sanitizer.
//!
//! [`TextChunker`] accumulates streamed LLM deltas and emits complete chunks
//! at sentence-end punctuation (`.!?\n`) with attached closing quotes/parens.
//! Rules (plan): a boundary only emits once the chunk is at least 30 chars
//! (unless the punct lands at `max_chars`); a run without punctuation longer
//! than `max_chars` is force-split at a word boundary so chunks stay bounded.
//! [`TextChunker::finish`] flushes the remainder.
//!
//! [`strip_for_speech`] removes markdown artifacts LLMs emit — fenced code
//! blocks (content suppressed entirely), inline backticks, link URLs (label
//! kept), leading `#` headers, bullet/dash markers, `*`/`_` emphasis — and
//! collapses whitespace, producing plain speakable text.

const MIN_CHUNK_CHARS: usize = 30;

/// True when `i` ends a sentence and the previous char isn't a decimal point.
fn is_boundary(b: &[char], i: usize) -> bool {
    matches!(b[i], '.' | '!' | '?' | '\n') && !(b[i] == '.' && i > 0 && b[i - 1].is_ascii_digit())
}

/// Run closing quotes/parens forward from a boundary so they stay attached.
fn boundary_end(b: &[char], i: usize) -> usize {
    let mut end = i + 1;
    while end < b.len() && matches!(b[end], '"' | '”' | '’' | ')' | ']') {
        end += 1;
    }
    end
}

/// Index just past trailing whitespace, so emitted chunks never end in spaces.
fn trim_end(b: &[char], end: usize) -> usize {
    let mut e = end;
    while e > 0 && b[e - 1].is_whitespace() {
        e -= 1;
    }
    e
}

/// Index of the first non-whitespace char at or after `start`.
fn trim_start(b: &[char], start: usize) -> usize {
    let mut s = start;
    while s < b.len() && b[s].is_whitespace() {
        s += 1;
    }
    s
}

/// Accumulates LLM text deltas and emits sentence-sized TTS chunks.
pub struct TextChunker {
    max_chars: usize,
    pending: String,
}

impl TextChunker {
    pub fn new(max_chars: usize) -> Self {
        Self {
            max_chars,
            pending: String::new(),
        }
    }

    pub fn push(&mut self, delta: &str) -> Vec<String> {
        self.pending.push_str(delta);
        self.emit_ready()
    }

    pub fn finish(&mut self) -> Vec<String> {
        let b = self.chars();
        let start = trim_start(&b, 0);
        let end = trim_end(&b, b.len());
        if start >= end {
            self.pending.clear(); // blank remainder
            return Vec::new();
        }
        self.take_range(start, end).into_iter().collect()
    }

    fn chars(&self) -> Vec<char> {
        self.pending.chars().collect()
    }

    /// Cut `[from, to)` out of `pending` and return it (skips empty/blank
    /// cuts — their bytes are still dropped).
    fn take_range(&mut self, from: usize, to: usize) -> Option<String> {
        if from >= to {
            return None;
        }
        let chunk: String = self.chars()[from..to].iter().collect();
        self.pending = self.chars()[to..].iter().collect();
        (!chunk.trim().is_empty()).then_some(chunk)
    }

    /// First boundary index whose qualifying chunk length is `>= min_chars`.
    fn qualifying_boundary(&self, b: &[char]) -> Option<usize> {
        for i in 0..b.len() {
            if is_boundary(b, i) {
                let end = trim_end(b, boundary_end(b, i));
                if end >= MIN_CHUNK_CHARS {
                    return Some(end);
                }
            }
        }
        None
    }

    /// Index (exclusive) cutting a run of at most `max_chars` chars at a word
    /// boundary; `None` when pending fits within `max_chars`.
    fn flush_cut(&self, b: &[char]) -> Option<usize> {
        if b.len() <= self.max_chars {
            return None;
        }
        let mut cut = self.max_chars;
        while cut > 0 && !b[cut - 1].is_whitespace() {
            cut -= 1;
        }
        if cut == 0 {
            return Some(self.max_chars); // one enormous word: hard cut
        }
        while cut < b.len() && b[cut].is_whitespace() {
            cut += 1; // leave the separator with the preceding chunk
        }
        Some(cut)
    }

    /// Drain `pending` into finished chunks per the boundary/min/flush rules.
    fn emit_ready(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        loop {
            let b = self.chars();
            let start = trim_start(&b, 0);
            match self.qualifying_boundary(&b) {
                Some(end) => {
                    if let Some(chunk) = self.take_range(start, end) {
                        out.push(chunk);
                    }
                }
                None => match self.flush_cut(&b) {
                    Some(cut) => {
                        let head = self.take_range(start, trim_end(&b, cut));
                        if let Some(c) = head {
                            out.push(c);
                        }
                    }
                    None => return out,
                },
            }
        }
    }
}

/// Convert markdown-ish LLM output into plain speakable text.
pub fn strip_for_speech(md: &str) -> String {
    // Line pass: drop fenced code blocks entirely, strip leading header
    // hashes and bullet/dash markers, then rejoin.
    let mut kept: Vec<&str> = Vec::new();
    let mut in_fence = false;
    for line in md.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        let t = line.trim();
        let t = t.trim_start_matches('#').trim_start();
        let t = t
            .strip_prefix("- ")
            .or_else(|| t.strip_prefix("* "))
            .or_else(|| t.strip_prefix("– "))
            .or_else(|| t.strip_prefix("— "))
            .or_else(|| t.strip_prefix("• "))
            .unwrap_or(t);
        kept.push(t);
    }
    let mut s = kept.join(" ");

    // Links: [label](url) → label (only real http(s) targets).
    while let Some(open) = s.find('[') {
        let Some(close) = s[open..].find("](").map(|i| open + i) else {
            break;
        };
        let Some(url_end) = s[close..].find(')').map(|i| close + i) else {
            break;
        };
        let url = &s[close + 2..url_end];
        if !url.starts_with("http://") && !url.starts_with("https://") {
            break;
        }
        let label = s[open + 1..close].to_string();
        s.replace_range(open..=url_end, &label);
    }
    // Inline code backticks and emphasis markers.
    s.retain(|c| !matches!(c, '`' | '*' | '_'));

    // Collapse all runs of whitespace.
    let mut out = String::with_capacity(s.len());
    let mut last_ws = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !last_ws && !out.is_empty() {
                out.push(' ');
            }
            last_ws = true;
        } else {
            out.push(c);
            last_ws = false;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_by_word_deltas_assemble_into_sentence_chunks() {
        let mut c = TextChunker::new(200);
        let text =
            "The quick brown fox jumps over the lazy dog. Then it went back to sleep for a while. ";
        let mut out = Vec::new();
        for w in text.split_inclusive(' ') {
            out.extend(c.push(w));
        }
        assert_eq!(
            out,
            vec![
                "The quick brown fox jumps over the lazy dog.".to_string(),
                "Then it went back to sleep for a while.".to_string(),
            ]
        );
        assert!(c.finish().is_empty());
    }

    #[test]
    fn short_sentences_wait_for_min_chars_then_flush_on_finish() {
        let mut c = TextChunker::new(200);
        assert!(c.push("Hi. ").is_empty(), "below min chars must not emit");
        assert!(c.push("Bye. ").is_empty(), "still below min chars");
        assert_eq!(c.finish(), vec!["Hi. Bye."]);
    }

    #[test]
    fn punctless_run_is_force_split_at_max_chars() {
        let mut c = TextChunker::new(50);
        let text = "alpha ".repeat(20); // 120 chars, no sentence punct
        let out = c.push(&text);
        assert_eq!(out.len(), 2, "120 chars > 50 must split: {out:?}");
        let rest = c.finish();
        assert_eq!(rest.len(), 1);
        for chunk in out.iter().chain(rest.iter()) {
            assert!(chunk.chars().count() <= 50, "chunk over max: {chunk:?}");
        }
        // no words lost or duplicated across the split
        let words: Vec<&str> = out
            .iter()
            .chain(rest.iter())
            .flat_map(|s| s.split_whitespace())
            .collect();
        assert_eq!(words.len(), 20);
        assert!(words.iter().all(|w| *w == "alpha"));
    }

    #[test]
    fn closing_quote_attaches_to_the_boundary() {
        let mut c = TextChunker::new(200);
        let out = c.push("He said \"Let's go right now please.\" and left.");
        assert_eq!(out, vec!["He said \"Let's go right now please.\""]);
        assert_eq!(c.finish(), vec!["and left."]);
    }

    #[test]
    fn decimal_point_is_not_a_sentence_boundary() {
        let mut c = TextChunker::new(200);
        let out = c.push("The temperature outside is about 72.5 degrees today in total.");
        assert_eq!(
            out,
            vec!["The temperature outside is about 72.5 degrees today in total."]
        );
        assert!(c.finish().is_empty());
    }

    #[test]
    fn newline_is_a_sentence_boundary() {
        let mut c = TextChunker::new(200);
        let out = c.push("First line here with some length to it.\nAnd more tail");
        assert_eq!(out, vec!["First line here with some length to it."]);
        assert_eq!(c.finish(), vec!["And more tail"]);
    }

    #[test]
    fn sentence_punct_at_or_over_max_emits_whole_sentence_in_one_push() {
        // Single-push case: the boundary rule (>= min or punct at max) emits
        // the complete sentence rather than splitting it mid-clause. Word-by-
        // word streaming splits via the flush rule instead (see the max test).
        let mut c = TextChunker::new(50);
        let s =
            "This is quite a long sentence without any punctuation at all until the very end here.";
        assert_eq!(c.push(s), vec![s]);
    }

    #[test]
    fn fenced_code_blocks_are_suppressed_from_speech() {
        let md = "Look here:\n```python\nprint('hello')\n```\nAll done now.";
        assert_eq!(strip_for_speech(md), "Look here: All done now.");
    }

    #[test]
    fn unclosed_fence_suppresses_rest_of_text() {
        assert_eq!(strip_for_speech("before\n```\nhidden stuff"), "before");
    }

    #[test]
    fn inline_code_keeps_content_drops_backticks() {
        assert_eq!(
            strip_for_speech("run `npm install` first"),
            "run npm install first"
        );
    }

    #[test]
    fn links_keep_label_only() {
        assert_eq!(
            strip_for_speech("see [the docs](https://example.com/x) for details"),
            "see the docs for details"
        );
    }

    #[test]
    fn headers_bullets_and_emphasis_are_stripped() {
        let md = "## Setup\n- first item\n- second item\n*bold* and _quiet_ words";
        assert_eq!(
            strip_for_speech(md),
            "Setup first item second item bold and quiet words"
        );
    }

    #[test]
    fn whitespace_is_collapsed() {
        assert_eq!(strip_for_speech("a\n\nb   c\t d"), "a b c d");
    }
}
