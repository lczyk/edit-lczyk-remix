use lsh::runtime::{ConflictTag, ParsedLine};
use stdext::arena::{Arena, scratch_arena};
use stdext::collections::BVec;

use crate::helpers::CoordType;
use crate::lsh::{HighlightKind, Highlighter, HighlighterState};

#[cfg(debug_assertions)]
const INTERVAL: CoordType = 16;
#[cfg(not(debug_assertions))]
const INTERVAL: CoordType = 1024;

/// The checkpoint rules are modelled in `doc/spec/lsh-cache.fizz`.
#[derive(Default)]
pub struct HighlighterCache {
    checkpoints: Vec<HighlighterState>,
    /// Counts checkpoint-restoring seeks so the coherence check below can run
    /// on a fraction of them -- it re-parses the file from the top.
    #[cfg(feature = "sanity")]
    seeks: u32,
}

/// One in this many checkpoint-restoring seeks gets verified against a full
/// re-parse. Low enough to stay interactive on a large file.
#[cfg(feature = "sanity")]
const VERIFY_EVERY: u32 = 32;

impl HighlighterCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Drop any cached states starting at (including) the given logical line.
    pub fn invalidate_from(&mut self, line: CoordType) {
        self.checkpoints.truncate(Self::ceil_line_to_offset(line));
    }

    /// Parse the given logical line. Past the end of the document the result
    /// has no spans and no conflict tag.
    pub fn parse_line<'a>(
        &mut self,
        arena: &'a Arena,
        highlighter: &mut Highlighter,
        line: CoordType,
    ) -> ParsedLine<'a, HighlightKind> {
        let seeked = line != highlighter.logical_pos_y();

        // Do we need to random seek?
        if seeked {
            // If so, restore the nearest, preceding checkpoint...
            if !self.checkpoints.is_empty() {
                let n = Self::floor_line_to_offset(line);
                let n = n.min(self.checkpoints.len() - 1);
                highlighter.restore(&self.checkpoints[n]);
            } else {
                // The assumption is that you pass in a default constructed highlighter,
                // and this class handles random seeking for you. As such, there should
                // never be a case where we don't have a checkpoint for line 0,
                // but you have a highlighter for line >0.
                crate::sanity_assert!(
                    highlighter_at_line_zero,
                    highlighter.logical_pos_y() == 0,
                    "highlighter.logical_pos_y={} (expected 0 when no checkpoint)",
                    highlighter.logical_pos_y()
                );
            }

            // ...and then seek in front of the requested line.
            while highlighter.logical_pos_y() < line {
                // There's a bit of waste here, because we just throw away the results,
                // but that's better than duplicating the logic. The arena is very fast.
                let scratch = scratch_arena(Some(arena));
                _ = self.parse_line_impl(&scratch, highlighter);
            }
        }

        let parsed = self.parse_line_impl(arena, highlighter);

        // Spans are half-open `[start, next.start)`, so a line's spans never
        // step backwards; a zero-width one is fine (an empty line is a start
        // and a sentinel at the same offset). A start behind the previous
        // one means the compiler pipeline emitted overlapping tokens, which
        // shows up only as odd-looking colour.
        #[cfg(feature = "sanity")]
        if let Some(bad) = parsed.spans.windows(2).position(|w| w[1].start < w[0].start) {
            crate::sanity_check!(
                highlighter_spans_monotonic,
                false,
                "line {line}: span {} starts at {} after {}",
                bad + 1,
                parsed.spans[bad + 1].start,
                parsed.spans[bad].start
            );
        }

        #[cfg(feature = "sanity")]
        if seeked {
            self.verify_against_full_reparse(arena, highlighter, line, &parsed);
        }

        parsed
    }

    /// Compares a checkpoint-restored parse against parsing the file from the
    /// top. The cache exists to avoid exactly that walk, so a mismatch means a
    /// checkpoint outlived the edit that should have invalidated it -- the
    /// stale-highlight class. Costs a full re-parse, hence [`VERIFY_EVERY`].
    #[cfg(feature = "sanity")]
    fn verify_against_full_reparse(
        &mut self,
        arena: &Arena,
        highlighter: &Highlighter,
        line: CoordType,
        parsed: &ParsedLine<'_, HighlightKind>,
    ) {
        self.seeks = self.seeks.wrapping_add(1);
        if !self.seeks.is_multiple_of(VERIFY_EVERY) {
            return;
        }

        // Without a line-0 checkpoint there is nothing to re-parse from: the
        // `highlighter_at_line_zero` assert above already covers that case.
        let Some(first) = self.checkpoints.first() else {
            return;
        };

        let scratch = scratch_arena(Some(arena));
        let mut fresh = highlighter.clone();
        fresh.restore(first);
        while fresh.logical_pos_y() < line {
            let inner = scratch_arena(Some(&scratch));
            _ = fresh.parse_next_line(&inner);
        }
        let expected = Self::past_the_end_or(fresh.parse_next_line(&scratch));

        crate::sanity_check!(
            highlighter_cache_coherent,
            expected.spans[..] == parsed.spans[..] && expected.conflict == parsed.conflict,
            "line {line}: cached {:?} {:?} != re-parsed {:?} {:?}",
            &parsed.spans[..],
            parsed.conflict,
            &expected.spans[..],
            expected.conflict
        );
    }

    fn past_the_end_or(
        parsed: Option<ParsedLine<'_, HighlightKind>>,
    ) -> ParsedLine<'_, HighlightKind> {
        parsed.unwrap_or(ParsedLine { spans: BVec::empty(), conflict: ConflictTag::None })
    }

    fn parse_line_impl<'a>(
        &mut self,
        arena: &'a Arena,
        highlighter: &mut Highlighter,
    ) -> ParsedLine<'a, HighlightKind> {
        // If we need to store a checkpoint for the start of the next line, do so now.
        if Self::floor_line_to_offset(highlighter.logical_pos_y()) == self.checkpoints.len() {
            self.checkpoints.push(highlighter.snapshot());
        }

        Self::past_the_end_or(highlighter.parse_next_line(arena))
    }

    /// Since this line cache is super simplistic (no insertions, only append),
    /// we can directly map from line numbers to offsets in the cache.
    fn floor_line_to_offset(line: CoordType) -> usize {
        (line / INTERVAL).try_into().unwrap_or(0)
    }

    fn ceil_line_to_offset(line: CoordType) -> usize {
        ((line + INTERVAL - 1) / INTERVAL).try_into().unwrap_or(0)
    }
}

#[cfg(all(test, feature = "sanity"))]
mod tests {
    use stdext::arena::Arena;
    use stdext::sanity::capture;

    use super::*;
    use crate::lsh::LANGUAGES;

    /// Enough lines to force at least one checkpoint under the debug INTERVAL,
    /// with content whose spans a stale checkpoint would get wrong: an unclosed
    /// block comment swallows everything after it, so a checkpoint carried over
    /// the wrong line would show up as a different span set.
    fn document() -> String {
        let mut doc = String::new();
        for i in 0..(INTERVAL * 3) {
            match i % 4 {
                0 => doc.push_str("fn f() { let s = \"str\"; }\n"),
                1 => doc.push_str("// a line comment\n"),
                2 => doc.push_str("/* an unterminated block comment\n"),
                _ => doc.push_str("still inside the comment */ let x = 1;\n"),
            }
        }
        doc
    }

    fn language() -> &'static lsh::runtime::Language {
        LANGUAGES.iter().find(|l| l.id == "rust").expect("rust is a bundled definition")
    }

    #[test]
    fn seeking_around_agrees_with_a_full_reparse() {
        // The coherence check normally samples one seek in VERIFY_EVERY; drive
        // enough of them that it is guaranteed to fire, and assert it stays
        // quiet. Walking backwards is what forces checkpoint restores.
        let arena = Arena::new(4 * 1024 * 1024).unwrap();
        let doc = document();
        let lang = language();

        let ((), msgs) = capture::trips(|| {
            let mut cache = HighlighterCache::new();
            let mut highlighter = Highlighter::new(&doc, lang);
            for round in 0..(VERIFY_EVERY as CoordType * 2) {
                let line = (INTERVAL * 3 - 1) - (round % (INTERVAL * 3));
                let scratch = stdext::arena::scratch_arena(Some(&arena));
                _ = cache.parse_line(&scratch, &mut highlighter, line);
            }
        });

        assert!(msgs.is_empty(), "{msgs:?}");
    }

    #[test]
    fn invalidating_then_reparsing_agrees_too() {
        // The failure mode the check exists for: a checkpoint that outlives the
        // edit which should have dropped it. Invalidate above the first
        // checkpoint so it survives -- dropping that one too is only legal
        // alongside a fresh highlighter, which is the case below.
        let arena = Arena::new(4 * 1024 * 1024).unwrap();
        let doc = document();
        let lang = language();

        let ((), msgs) = capture::trips(|| {
            let mut cache = HighlighterCache::new();
            let mut highlighter = Highlighter::new(&doc, lang);
            for round in 0..(VERIFY_EVERY as CoordType * 2) {
                let scratch = stdext::arena::scratch_arena(Some(&arena));
                _ = cache.parse_line(&scratch, &mut highlighter, INTERVAL * 2 + round % INTERVAL);
                cache.invalidate_from(INTERVAL + round % INTERVAL);
                let scratch = stdext::arena::scratch_arena(Some(&arena));
                _ = cache.parse_line(&scratch, &mut highlighter, INTERVAL + round % INTERVAL);
            }
        });

        assert!(msgs.is_empty(), "{msgs:?}");
    }

    #[test]
    fn a_full_invalidation_needs_a_fresh_highlighter() {
        // `invalidate_from(0)` empties the checkpoints, and a seek with none of
        // them left is only meaningful from line 0 -- `highlighter_at_line_zero`
        // asserts exactly that. The render path satisfies it by building a new
        // Highlighter per pass (see buffer/render.rs), so mirror that here.
        let arena = Arena::new(4 * 1024 * 1024).unwrap();
        let doc = document();
        let lang = language();

        let ((), msgs) = capture::trips(|| {
            let mut cache = HighlighterCache::new();
            for round in 0..(VERIFY_EVERY as CoordType * 2) {
                let mut highlighter = Highlighter::new(&doc, lang);
                let scratch = stdext::arena::scratch_arena(Some(&arena));
                _ = cache.parse_line(&scratch, &mut highlighter, INTERVAL * 2 + round % INTERVAL);
                cache.invalidate_from(0);
            }
        });

        assert!(msgs.is_empty(), "{msgs:?}");
    }
}
