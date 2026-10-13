use std::ops::Range;
use std::ptr::{self, NonNull};
use std::{io, slice};

use stdext::sys::{virtual_commit, virtual_release, virtual_reserve};
use stdext::{ReplaceRange as _, slice_copy_safe};

use crate::document::{ReadableDocument, WriteableDocument};
use crate::helpers::*;

#[cfg(target_pointer_width = "32")]
const LARGE_CAPACITY: usize = 128 * MEBI;
#[cfg(target_pointer_width = "64")]
const LARGE_CAPACITY: usize = 4 * GIBI;
const LARGE_ALLOC_CHUNK: usize = 64 * KIBI;
const LARGE_GAP_CHUNK: usize = 4 * KIBI;

const SMALL_CAPACITY: usize = 128 * KIBI;
const SMALL_ALLOC_CHUNK: usize = 256;
const SMALL_GAP_CHUNK: usize = 16;

// TODO: Instead of having a specialization for small buffers here,
// tui.rs could also just keep a MRU set of large buffers around.
enum BackingBuffer {
    VirtualMemory(NonNull<u8>, usize),
    Vec(Vec<u8>),
}

impl Drop for BackingBuffer {
    fn drop(&mut self) {
        unsafe {
            if let Self::VirtualMemory(ptr, reserve) = *self {
                virtual_release(ptr, reserve);
            }
        }
    }
}

/// Most people know how `Vec<T>` works: It has some spare capacity at the end,
/// so that pushing into it doesn't reallocate every single time. A gap buffer
/// is the same thing, but the spare capacity can be anywhere in the buffer.
/// This variant is optimized for large buffers and uses virtual memory.
pub struct GapBuffer {
    /// Pointer to the buffer.
    text: NonNull<u8>,
    /// Maximum size of the buffer, including gap.
    reserve: usize,
    /// Size of the buffer, including gap.
    commit: usize,
    /// Length of the stored text, NOT including gap.
    text_length: usize,
    /// Gap offset.
    gap_off: usize,
    /// Gap length.
    gap_len: usize,
    /// Increments every time the buffer is modified.
    generation: u32,
    /// If `Vec(..)`, the buffer is optimized for small amounts of text
    /// and uses the standard heap. Otherwise, it uses virtual memory.
    buffer: BackingBuffer,
}

/// A complete edit to a [`GapBuffer`], with the intent stated rather than
/// encoded in the range.
///
/// "Replace to the end" used to be spelled `off..usize::MAX` and "append"
/// `usize::MAX..usize::MAX`, which meant an out-of-range range could be either
/// a deliberate idiom or a caller's mistake, and so could not be checked.
#[derive(Debug)]
pub enum Edit<'a> {
    /// Insert at `at`, shifting the rest along.
    Insert { at: usize, text: &'a [u8] },
    /// Remove `range`.
    Delete { range: Range<usize> },
    /// Replace `range` with `text`.
    Replace { range: Range<usize>, text: &'a [u8] },
    /// Replace everything from `from` onwards with `text`.
    ReplaceToEnd { from: usize, text: &'a [u8] },
}

impl GapBuffer {
    pub fn new(small: bool) -> io::Result<Self> {
        let reserve;
        let buffer;
        let text;

        if small {
            reserve = SMALL_CAPACITY;
            text = NonNull::dangling();
            buffer = BackingBuffer::Vec(Vec::new());
        } else {
            reserve = LARGE_CAPACITY;
            text = unsafe { virtual_reserve(reserve)? };
            buffer = BackingBuffer::VirtualMemory(text, reserve);
        }

        Ok(Self {
            text,
            reserve,
            commit: 0,
            text_length: 0,
            gap_off: 0,
            gap_len: 0,
            generation: 0,
            buffer,
        })
    }

    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.text_length
    }

    pub fn generation(&self) -> u32 {
        self.generation
    }

    pub fn set_generation(&mut self, generation: u32) {
        self.generation = generation;
    }

    /// Makes room at `off` for `len` bytes, removing `delete` of the existing
    /// ones, and hands back the space to write into.
    ///
    /// The raw door, for the two callers that fill the space incrementally: a
    /// streaming file read and undo's reinsert loop. Everything else should
    /// state its intent with [`Edit`] and go through [`GapBuffer::apply`].
    ///
    /// WARNING: The returned slice must not necessarily be the same length as `len` (due to OOM).
    pub fn reserve_gap(&mut self, off: usize, len: usize, delete: usize) -> &mut [u8] {
        // Out-of-range arguments are clamped rather than refused. Callers going
        // through `apply` have already been checked; `replace` is permissive by
        // contract, matching `WriteableDocument::replace`.
        //
        // Sanitize parameters
        let off = off.min(self.text_length);
        let delete = delete.min(self.text_length - off);

        // Move the existing gap if it exists
        if off != self.gap_off {
            self.move_gap(off);
        }

        // Delete the text
        if delete > 0 {
            self.delete_text(delete);
        }

        // Enlarge the gap if needed
        if len > self.gap_len {
            self.enlarge_gap(len);
        }

        self.generation = self.generation.wrapping_add(1);
        unsafe { slice::from_raw_parts_mut(self.text.add(self.gap_off).as_ptr(), self.gap_len) }
    }

    fn move_gap(&mut self, off: usize) {
        if self.gap_len > 0 {
            //
            //                       v gap_off
            // left:  |ABCDEFGHIJKLMN   OPQRSTUVWXYZ|
            //        |ABCDEFGHI   JKLMNOPQRSTUVWXYZ|
            //                  ^ off
            //        move: JKLMN
            //
            //                       v gap_off
            // !left: |ABCDEFGHIJKLMN   OPQRSTUVWXYZ|
            //        |ABCDEFGHIJKLMNOPQRS   TUVWXYZ|
            //                            ^ off
            //        move: OPQRS
            //
            let left = off < self.gap_off;
            let move_src = if left { off } else { self.gap_off + self.gap_len };
            let move_dst = if left { off + self.gap_len } else { self.gap_off };
            let move_len = if left { self.gap_off - off } else { off - self.gap_off };

            unsafe { self.text.add(move_src).copy_to(self.text.add(move_dst), move_len) };

            if cfg!(debug_assertions) {
                // Fill the moved-out bytes with 0xCD to make debugging easier.
                unsafe { self.text.add(off).write_bytes(0xCD, self.gap_len) };
            }
        }

        self.gap_off = off;
    }

    fn delete_text(&mut self, delete: usize) {
        if cfg!(debug_assertions) {
            // Fill the deleted bytes with 0xCD to make debugging easier.
            unsafe { self.text.add(self.gap_off + self.gap_len).write_bytes(0xCD, delete) };
        }

        self.gap_len += delete;
        self.text_length -= delete;
    }

    fn enlarge_gap(&mut self, len: usize) {
        let (gap_chunk, alloc_chunk) = if matches!(self.buffer, BackingBuffer::VirtualMemory(..)) {
            (LARGE_GAP_CHUNK, LARGE_ALLOC_CHUNK)
        } else {
            (SMALL_GAP_CHUNK, SMALL_ALLOC_CHUNK)
        };

        let gap_len_old = self.gap_len;
        let gap_len_new = (len + gap_chunk + gap_chunk - 1) & !(gap_chunk - 1);

        let bytes_old = self.commit;
        let bytes_new = self.text_length + gap_len_new;

        if bytes_new > bytes_old {
            let bytes_new = (bytes_new + alloc_chunk - 1) & !(alloc_chunk - 1);

            if bytes_new > self.reserve {
                return;
            }

            match &mut self.buffer {
                BackingBuffer::VirtualMemory(ptr, _) => unsafe {
                    if virtual_commit(ptr.add(bytes_old), bytes_new - bytes_old).is_err() {
                        return;
                    }
                },
                BackingBuffer::Vec(v) => {
                    v.resize(bytes_new, 0);
                    self.text = unsafe { NonNull::new_unchecked(v.as_mut_ptr()) };
                }
            }

            self.commit = bytes_new;
        }

        let gap_beg = unsafe { self.text.add(self.gap_off) };
        unsafe {
            ptr::copy(
                gap_beg.add(gap_len_old).as_ptr(),
                gap_beg.add(gap_len_new).as_ptr(),
                self.text_length - self.gap_off,
            )
        };

        if cfg!(debug_assertions) {
            // Fill the moved-out bytes with 0xCD to make debugging easier.
            unsafe { gap_beg.add(gap_len_old).write_bytes(0xCD, gap_len_new - gap_len_old) };
        }

        self.gap_len = gap_len_new;
    }

    pub fn commit_gap(&mut self, len: usize) {
        assert!(len <= self.gap_len);
        self.text_length += len;
        self.gap_off += len;
        self.gap_len -= len;
    }

    /// Replaces `range` with `src`, clamping an out-of-range range.
    ///
    /// Permissive on purpose, mirroring [`crate::document::WriteableDocument`],
    /// whose contract says a range may be out of bounds and must be clamped.
    /// Prefer [`GapBuffer::apply`] inside the buffer, where the intent can be
    /// stated and therefore checked.
    pub fn replace(&mut self, range: Range<usize>, src: &[u8]) -> usize {
        let gap = self.reserve_gap(range.start, src.len(), range.end.saturating_sub(range.start));
        let len = slice_copy_safe(gap, src);
        self.commit_gap(len);
        len
    }

    /// Applies a stated edit, returning how many bytes of its text were written.
    ///
    /// Fewer than the text's length means the allocation came up short, which
    /// only happens under OOM.
    pub fn apply(&mut self, edit: Edit<'_>) -> usize {
        let (off, delete, text) = match edit {
            Edit::Insert { at, text } => (at, 0, text),
            Edit::Delete { ref range } => {
                (range.start, range.end.saturating_sub(range.start), &[][..])
            }
            Edit::Replace { ref range, text } => {
                (range.start, range.end.saturating_sub(range.start), text)
            }
            Edit::ReplaceToEnd { from, text } => {
                (from, self.text_length.saturating_sub(from), text)
            }
        };

        // Checkable at last. While "replace to the end" and "append" were spelled
        // as an out-of-range range, a caller's arithmetic slip was
        // indistinguishable from the idiom, so this could not be asserted.
        crate::sanity_check!(
            gap_edit_in_bounds,
            off <= self.text_length && off.saturating_add(delete) <= self.text_length,
            "{edit:?} against a buffer of {} bytes",
            self.text_length
        );

        let gap = self.reserve_gap(off, text.len(), delete);
        let written = slice_copy_safe(gap, text);
        self.commit_gap(written);
        written
    }

    pub fn clear(&mut self) {
        self.gap_off = 0;
        self.gap_len += self.text_length;
        self.generation = self.generation.wrapping_add(1);
        self.text_length = 0;
    }

    pub fn extract_raw(&self, range: Range<usize>, out: &mut Vec<u8>, mut out_off: usize) {
        let end = range.end.min(self.text_length);
        let mut beg = range.start.min(end);
        out_off = out_off.min(out.len());

        if beg >= end {
            return;
        }

        out.reserve(end - beg);

        while beg < end {
            let chunk = self.read_forward(beg);
            let chunk = &chunk[..chunk.len().min(end - beg)];
            out.replace_range(out_off..out_off, chunk);
            beg += chunk.len();
            out_off += chunk.len();
        }
    }

    /// Replaces the entire buffer contents with the given `text`.
    /// The method is optimized for the case where the given `text` already matches
    /// the existing contents. Returns `true` if the buffer contents were changed.
    pub fn copy_from(&mut self, src: &dyn ReadableDocument) -> bool {
        let mut off = 0;

        // Find the position at which the contents change.
        loop {
            let dst_chunk = self.read_forward(off);
            let src_chunk = src.read_forward(off);

            let dst_len = dst_chunk.len();
            let src_len = src_chunk.len();
            let len = dst_len.min(src_len);
            let mismatch = dst_chunk[..len] != src_chunk[..len];

            if mismatch {
                break; // The contents differ.
            }
            if len == 0 {
                if dst_len == src_len {
                    return false; // Both done simultaneously. -> Done.
                }
                break; // One of the two is shorter.
            }

            off += len;
        }

        // Update the buffer starting at `off`.
        loop {
            let chunk = src.read_forward(off);
            self.apply(Edit::ReplaceToEnd { from: off, text: chunk });
            off += chunk.len();

            // No more data to copy -> Done. By checking this _after_ the replace()
            // call, we ensure that the initial `off..usize::MAX` range is deleted.
            // This fixes going from some buffer contents to being empty.
            if chunk.is_empty() {
                return true;
            }
        }
    }

    /// Copies the contents of the buffer into a string.
    pub fn copy_into(&self, dst: &mut dyn WriteableDocument) {
        let mut beg = 0;
        let mut off = 0;

        while {
            let chunk = self.read_forward(off);

            // The first write will be 0..usize::MAX and effectively clear() the destination.
            // Every subsequent write will be usize::MAX..usize::MAX and thus effectively append().
            dst.replace(beg..usize::MAX, chunk);
            beg = usize::MAX;

            off += chunk.len();
            off < self.text_length
        } {}
    }
}

impl ReadableDocument for GapBuffer {
    fn read_forward(&self, off: usize) -> &[u8] {
        let off = off.min(self.text_length);
        let (beg, len) = if off < self.gap_off {
            // Cursor is before the gap: We can read until the start of the gap.
            (off, self.gap_off - off)
        } else {
            // Cursor is after the gap: We can read until the end of the buffer.
            (off + self.gap_len, self.text_length - off)
        };

        unsafe { slice::from_raw_parts(self.text.add(beg).as_ptr(), len) }
    }

    fn read_backward(&self, off: usize) -> &[u8] {
        let off = off.min(self.text_length);
        let (beg, len) = if off <= self.gap_off {
            // Cursor is before the gap: We can read until the beginning of the buffer.
            (0, off)
        } else {
            // Cursor is after the gap: We can read until the end of the gap.
            // The cursor_off doesn't account of the gap_len.
            // (This allows us to move the gap without recalculating the cursor position.)
            (self.gap_off + self.gap_len, off - self.gap_off)
        };

        unsafe { slice::from_raw_parts(self.text.add(beg).as_ptr(), len) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(b: &GapBuffer) -> Vec<u8> {
        let mut out = Vec::new();
        let mut off = 0;
        while off < b.len() {
            let chunk = b.read_forward(off);
            assert!(!chunk.is_empty(), "read_forward must not return empty before end");
            out.extend_from_slice(chunk);
            off += chunk.len();
        }
        out
    }

    fn collect_backward(b: &GapBuffer) -> Vec<u8> {
        let mut out = Vec::new();
        let mut off = b.len();
        while off > 0 {
            let chunk = b.read_backward(off);
            assert!(!chunk.is_empty(), "read_backward must not return empty before start");
            // Prepend (chunks come in reverse order).
            let mut chunk_owned = chunk.to_vec();
            chunk_owned.extend_from_slice(&out);
            out = chunk_owned;
            off -= chunk.len();
        }
        out
    }

    #[test]
    fn new_small_is_empty() {
        let b = GapBuffer::new(true).unwrap();
        assert_eq!(b.len(), 0);
        assert_eq!(b.generation(), 0);
        assert_eq!(collect(&b), b"");
    }

    #[test]
    fn new_large_is_empty() {
        let b = GapBuffer::new(false).unwrap();
        assert_eq!(b.len(), 0);
        assert_eq!(collect(&b), b"");
    }

    #[test]
    fn replace_inserts_at_start() {
        let mut b = GapBuffer::new(true).unwrap();
        b.replace(0..0, b"hello");
        assert_eq!(b.len(), 5);
        assert_eq!(collect(&b), b"hello");
    }

    #[test]
    fn replace_appends_at_end() {
        let mut b = GapBuffer::new(true).unwrap();
        b.replace(0..0, b"hello");
        b.replace(5..5, b" world");
        assert_eq!(collect(&b), b"hello world");
    }

    #[test]
    fn replace_inserts_in_middle() {
        let mut b = GapBuffer::new(true).unwrap();
        b.replace(0..0, b"helloworld");
        b.replace(5..5, b", ");
        assert_eq!(collect(&b), b"hello, world");
    }

    #[test]
    fn replace_deletes_range() {
        let mut b = GapBuffer::new(true).unwrap();
        b.replace(0..0, b"abcdef");
        b.replace(2..4, b"");
        assert_eq!(collect(&b), b"abef");
    }

    #[test]
    fn replace_substitutes_range() {
        let mut b = GapBuffer::new(true).unwrap();
        b.replace(0..0, b"abcdef");
        b.replace(2..4, b"XY");
        assert_eq!(collect(&b), b"abXYef");
    }

    #[test]
    fn replace_substitutes_with_longer() {
        let mut b = GapBuffer::new(true).unwrap();
        b.replace(0..0, b"abcdef");
        b.replace(2..4, b"WXYZ");
        assert_eq!(collect(&b), b"abWXYZef");
    }

    #[test]
    fn apply_states_the_intent_that_a_range_used_to_encode() {
        let mut b = GapBuffer::new(true).unwrap();
        b.apply(Edit::Insert { at: 0, text: b"abcdef" });
        assert_eq!(collect(&b), b"abcdef");

        b.apply(Edit::Delete { range: 1..3 });
        assert_eq!(collect(&b), b"adef");

        b.apply(Edit::Replace { range: 1..2, text: b"XY" });
        assert_eq!(collect(&b), b"aXYef");

        // What `off..usize::MAX` used to mean.
        b.apply(Edit::ReplaceToEnd { from: 2, text: b"Z" });
        assert_eq!(collect(&b), b"aXZ");

        // And what an empty text at the end means: a truncation.
        b.apply(Edit::ReplaceToEnd { from: 1, text: b"" });
        assert_eq!(collect(&b), b"a");
    }

    #[test]
    fn apply_reports_how_much_it_wrote() {
        let mut b = GapBuffer::new(true).unwrap();
        assert_eq!(b.apply(Edit::Insert { at: 0, text: b"hello" }), 5);
        assert_eq!(b.apply(Edit::Delete { range: 0..2 }), 0);
    }

    #[cfg(feature = "sanity")]
    #[test]
    fn apply_notices_an_out_of_range_edit() {
        use stdext::sanity::capture;

        let mut b = GapBuffer::new(true).unwrap();
        b.apply(Edit::Insert { at: 0, text: b"abc" });

        // The whole point of the enum: this is now distinguishable from the
        // deliberate "to the end" idiom, which used to look identical.
        let ((), msgs) = capture::trips(|| {
            b.apply(Edit::Delete { range: 2..99 });
        });
        assert!(capture::fired(&msgs, "gap_edit_in_bounds"), "{msgs:?}");

        // ...while the idiom itself stays quiet.
        let ((), msgs) = capture::trips(|| {
            b.apply(Edit::ReplaceToEnd { from: 1, text: b"Z" });
        });
        assert!(msgs.is_empty(), "{msgs:?}");
    }

    #[test]
    fn replace_substitutes_with_shorter() {
        let mut b = GapBuffer::new(true).unwrap();
        b.replace(0..0, b"abcdefghij");
        b.replace(2..8, b"X");
        assert_eq!(collect(&b), b"abXij");
    }

    #[test]
    fn replace_out_of_bounds_clamped() {
        let mut b = GapBuffer::new(true).unwrap();
        b.replace(0..0, b"abc");
        // Range past end should clamp.
        b.replace(2..1000, b"");
        assert_eq!(collect(&b), b"ab");
    }

    #[test]
    fn replace_start_past_end_appends() {
        let mut b = GapBuffer::new(true).unwrap();
        b.replace(0..0, b"abc");
        b.replace(1000..1000, b"X");
        assert_eq!(collect(&b), b"abcX");
    }

    #[test]
    fn clear_empties_buffer() {
        let mut b = GapBuffer::new(true).unwrap();
        b.replace(0..0, b"hello");
        b.clear();
        assert_eq!(b.len(), 0);
        assert_eq!(collect(&b), b"");
    }

    #[test]
    fn clear_then_insert_works() {
        let mut b = GapBuffer::new(true).unwrap();
        b.replace(0..0, b"hello");
        b.clear();
        b.replace(0..0, b"world");
        assert_eq!(collect(&b), b"world");
    }

    #[test]
    fn generation_changes_on_mutation() {
        let mut b = GapBuffer::new(true).unwrap();
        let g0 = b.generation();
        b.replace(0..0, b"abc");
        let g1 = b.generation();
        assert_ne!(g0, g1, "replace should bump generation");
        b.clear();
        let g2 = b.generation();
        assert_ne!(g1, g2, "clear should bump generation");
    }

    #[test]
    fn set_generation_roundtrips() {
        let mut b = GapBuffer::new(true).unwrap();
        b.set_generation(42);
        assert_eq!(b.generation(), 42);
    }

    #[test]
    fn copy_from_fills_buffer() {
        let mut b = GapBuffer::new(true).unwrap();
        let src: &[u8] = b"the quick brown fox";
        let changed = b.copy_from(&src);
        assert!(changed);
        assert_eq!(collect(&b), src);
    }

    #[test]
    fn copy_from_identical_returns_false() {
        let mut b = GapBuffer::new(true).unwrap();
        let src: &[u8] = b"hello world";
        b.copy_from(&src);
        let changed = b.copy_from(&src);
        assert!(!changed, "copying identical content should report no change");
        assert_eq!(collect(&b), src);
    }

    #[test]
    fn copy_from_different_returns_true() {
        let mut b = GapBuffer::new(true).unwrap();
        b.copy_from(&(b"hello".as_ref()));
        let changed = b.copy_from(&(b"world".as_ref()));
        assert!(changed);
        assert_eq!(collect(&b), b"world");
    }

    #[test]
    fn copy_from_to_empty() {
        let mut b = GapBuffer::new(true).unwrap();
        b.copy_from(&(b"non-empty".as_ref()));
        let empty: &[u8] = b"";
        let changed = b.copy_from(&empty);
        assert!(changed);
        assert_eq!(b.len(), 0);
        assert_eq!(collect(&b), b"");
    }

    #[test]
    fn copy_into_string() {
        let mut b = GapBuffer::new(true).unwrap();
        b.replace(0..0, b"hello, world");
        let mut dst = String::new();
        b.copy_into(&mut dst);
        assert_eq!(dst, "hello, world");
    }

    #[test]
    fn copy_into_overwrites_destination() {
        let mut b = GapBuffer::new(true).unwrap();
        b.replace(0..0, b"new");
        let mut dst = String::from("pre-existing content");
        b.copy_into(&mut dst);
        assert_eq!(dst, "new");
    }

    #[test]
    fn extract_raw_full_range() {
        let mut b = GapBuffer::new(true).unwrap();
        b.replace(0..0, b"abcdef");
        let mut out = Vec::new();
        b.extract_raw(0..b.len(), &mut out, 0);
        assert_eq!(out, b"abcdef");
    }

    #[test]
    fn extract_raw_partial_range() {
        let mut b = GapBuffer::new(true).unwrap();
        b.replace(0..0, b"abcdef");
        let mut out = Vec::new();
        b.extract_raw(2..5, &mut out, 0);
        assert_eq!(out, b"cde");
    }

    #[test]
    fn extract_raw_empty_range() {
        let mut b = GapBuffer::new(true).unwrap();
        b.replace(0..0, b"abcdef");
        let mut out = Vec::new();
        b.extract_raw(3..3, &mut out, 0);
        assert_eq!(out, b"");
    }

    #[test]
    fn extract_raw_clamps_oob() {
        let mut b = GapBuffer::new(true).unwrap();
        b.replace(0..0, b"abc");
        let mut out = Vec::new();
        b.extract_raw(1..1000, &mut out, 0);
        assert_eq!(out, b"bc");
    }

    #[test]
    fn extract_raw_inserts_at_offset() {
        let mut b = GapBuffer::new(true).unwrap();
        b.replace(0..0, b"abcdef");
        let mut out = b"___".to_vec();
        b.extract_raw(0..3, &mut out, 1);
        // Insert "abc" at index 1 of "___" -> "_abc__"
        assert_eq!(out, b"_abc__");
    }

    #[test]
    fn read_backward_concatenates_to_content() {
        let mut b = GapBuffer::new(true).unwrap();
        b.replace(0..0, b"hello, world");
        // Force a gap by inserting in the middle.
        b.replace(5..5, b"XYZ");
        let expected = b"helloXYZ, world".to_vec();
        assert_eq!(collect(&b), expected);
        assert_eq!(collect_backward(&b), expected);
    }

    #[test]
    fn read_forward_past_end_returns_empty() {
        let mut b = GapBuffer::new(true).unwrap();
        b.replace(0..0, b"abc");
        let chunk = b.read_forward(b.len());
        assert!(chunk.is_empty());
        let chunk = b.read_forward(b.len() + 100);
        assert!(chunk.is_empty());
    }

    #[test]
    fn read_backward_at_zero_returns_empty() {
        let mut b = GapBuffer::new(true).unwrap();
        b.replace(0..0, b"abc");
        let chunk = b.read_backward(0);
        assert!(chunk.is_empty());
    }

    #[test]
    fn many_small_inserts() {
        let mut b = GapBuffer::new(true).unwrap();
        let mut model = Vec::new();
        for i in 0..256u32 {
            let byte = (i & 0xff) as u8;
            let off = i as usize % (model.len() + 1);
            b.replace(off..off, &[byte]);
            model.insert(off, byte);
        }
        assert_eq!(collect(&b), model);
    }

    #[test]
    fn large_insert_grows_buffer() {
        let mut b = GapBuffer::new(true).unwrap();
        let payload = vec![b'A'; 10_000];
        b.replace(0..0, &payload);
        assert_eq!(b.len(), 10_000);
        assert_eq!(collect(&b), payload);
    }

    #[test]
    fn insert_then_delete_then_insert_roundtrip() {
        let mut b = GapBuffer::new(true).unwrap();
        b.replace(0..0, b"abcdefghij");
        b.replace(3..7, b""); // remove "defg" -> "abchij"
        b.replace(3..3, b"XYZ"); // insert at 3 -> "abcXYZhij"
        assert_eq!(collect(&b), b"abcXYZhij");
    }
}
