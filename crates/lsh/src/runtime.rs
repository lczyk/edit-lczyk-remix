//! LSH bytecode interpreter.
//!
//! ## Performance notes
//!
//! - The main loop is "unsafe". Profile before "cleaning" it up.
//! - `charset_gobble`, `inlined_mem(i)cmp` are hot paths.
//!
//! ## Instruction encoding
//!
//! Variable-length encoding, 1-9 bytes per instruction. See [`Instruction::encode`].
//!
//! ## Gotchas
//!
//! - `Return` with empty stack resets the VM to entrypoint and clears registers.
//!   This is how the DSL returns to the "idle" state between tokens.
//! - `AwaitInput` only breaks the loop if `off >= line.len()`. If not at EOL, it's a no-op.
//!   This allows the DSL to say "wait for more input OR continue if there is some".
//! - The result always has a sentinel span at `line.len()`. Consumers can rely on this.
//! - [`Instruction::address_offset`] returns where, within an instruction, the jump target lives,
//!   as used by the backend's relocation system.

use std::fmt::{self, Debug};
use std::mem;

use stdext::arena::Arena;
use stdext::arena_write_fmt;
use stdext::collections::{BString, BVec};

use crate::conflict::ConflictState;
pub use crate::conflict::ConflictTag;
use crate::kind;

/// ANSI-16 colour identifier. Used by [`HighlightKind::default_color`] (the
/// generated method) to express the canonical default colour for each
/// highlight kind, decoupled from any specific output format. Consumers map
/// to their target representation (ANSI SGR escape, indexed-palette entry,
/// truecolor RGB, etc.).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Ansi16 {
    Black = 0,
    Red = 1,
    Green = 2,
    Yellow = 3,
    Blue = 4,
    Magenta = 5,
    Cyan = 6,
    White = 7,
    BrightBlack = 8,
    BrightRed = 9,
    BrightGreen = 10,
    BrightYellow = 11,
    BrightBlue = 12,
    BrightMagenta = 13,
    BrightCyan = 14,
    BrightWhite = 15,
}

impl Ansi16 {
    /// The SGR escape that selects this colour as the foreground.
    ///
    /// 30-37 for the first eight, 90-97 for the bright half. Provided so
    /// every consumer that wants ansi output maps the canonical table the
    /// same way instead of transcribing it.
    pub fn sgr(self) -> &'static str {
        match self {
            Ansi16::Black => "\x1b[30m",
            Ansi16::Red => "\x1b[31m",
            Ansi16::Green => "\x1b[32m",
            Ansi16::Yellow => "\x1b[33m",
            Ansi16::Blue => "\x1b[34m",
            Ansi16::Magenta => "\x1b[35m",
            Ansi16::White => "\x1b[37m",
            Ansi16::Cyan => "\x1b[36m",
            Ansi16::BrightBlack => "\x1b[90m",
            Ansi16::BrightRed => "\x1b[91m",
            Ansi16::BrightGreen => "\x1b[92m",
            Ansi16::BrightYellow => "\x1b[93m",
            Ansi16::BrightBlue => "\x1b[94m",
            Ansi16::BrightMagenta => "\x1b[95m",
            Ansi16::BrightCyan => "\x1b[96m",
            Ansi16::BrightWhite => "\x1b[97m",
        }
    }
}

/// A compiled language definition with its bytecode entrypoint.
pub struct Language {
    /// Unique identifier (e.g., "rust", "markdown").
    pub id: &'static str,
    /// Human-readable display name.
    pub name: &'static str,
    /// Line-comment token (e.g., "//" for rust, "#" for python). `None` if
    /// the language has no line-comment syntax.
    pub line_comment: Option<&'static str>,
    /// Block-comment open/close pair (e.g., `("/*", "*/")` for rust,
    /// `("<!--", "-->")` for markdown/html). `None` if the language has no
    /// block-comment syntax.
    pub block_comment: Option<(&'static str, &'static str)>,
    /// Shebang interpreter tokens for automatic language detection
    /// (e.g., `["python", "python3"]` for python).
    pub shebangs: &'static [&'static str],
    /// Bytecode address where execution begins for this language.
    pub entrypoint: u32,
    /// Bytecode address of an optional content-sniff detector. When `Some`,
    /// the resolver can call [`Runtime::detect`] to disambiguate against
    /// other path-glob candidates -- e.g. dialect-of-yaml definitions sharing
    /// the same `*.yaml` glob. `None` means "always wins when its glob hits"
    /// (the base-language fallback).
    pub detect_entrypoint: Option<u32>,
}

impl PartialEq for &'static Language {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(*self, *other)
    }
}

/// A highlight span indicating that text from `start` to the next span has the given `kind`.
///
/// Spans are half-open: `[start, next.start)`. The final span in a line extends to EOL.
#[derive(Clone, PartialEq, Eq)]
pub struct Highlight<T> {
    /// Byte offset where this highlight begins.
    pub start: usize,
    /// The token/highlight type (e.g., keyword, string, comment).
    pub kind: T,
}

impl<T: Debug> Debug for Highlight<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "({}, {:?})", self.start, self.kind)
    }
}

/// The bytecode interpreter for syntax highlighting.
#[derive(Clone)]
pub struct Runtime<'pa, 'ps, 'pc> {
    assembly: &'pa [u8],
    strings: &'ps [&'ps str],
    charsets: &'pc [[u16; 16]],
    entrypoint: u32,
    state: RuntimeState,
}

/// Everything a line leaves behind for the next one. Cloned whole for the
/// snapshots edit's incremental re-highlighting keeps.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct RuntimeState {
    vm: VmState,
    conflict: ConflictState,
}

/// One parsed line: its highlight spans and where it sits relative to a
/// merge conflict.
pub struct ParsedLine<'a, T> {
    pub spans: BVec<'a, Highlight<T>>,
    pub conflict: ConflictTag,
}

/// The interpreter's own state: the call stack, the registers, and the span
/// a definition asked to remember.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct VmState {
    stack: Vec<u32>,
    registers: Registers,
    saved: SavedSpan,
}

impl VmState {
    /// Back to idle at `entrypoint`: no call frames, registers cleared. The
    /// saved span stays, as it does across a top-level return.
    fn reset_to(&mut self, entrypoint: u32) {
        self.stack.clear();
        self.registers = Registers { pc: entrypoint, ..Default::default() };
    }

    /// Whether two states are inside the same construct: equal up to the
    /// per-line registers (off, hs, ln), which only say where on its last
    /// line each vm stopped.
    pub fn same_construct(&self, other: &Self) -> bool {
        self.stack == other.stack
            && self.saved == other.saved
            && self.registers.cross_line() == other.registers.cross_line()
    }
}

/// Bytes a definition asked to remember with `save $N`, so a later line can
/// test for them with `if $saved` -- a heredoc delimiter, typically. Lives
/// outside the registers because it is text, not a number, and outlives the
/// line it was captured from. Inline and fixed-size so snapshots stay cheap;
/// anything longer than the buffer is cut, and a cut span never matches.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct SavedSpan {
    len: u8,
    cut: bool,
    bytes: [u8; Self::CAPACITY],
}

impl SavedSpan {
    const CAPACITY: usize = 30;

    fn set(&mut self, line: &[u8], start: u32, end: u32) {
        let start = (start as usize).min(line.len());
        let end = (end as usize).clamp(start, line.len());
        let span = &line[start..end];
        let len = span.len().min(Self::CAPACITY);
        self.bytes[..len].copy_from_slice(&span[..len]);
        self.len = len as u8;
        self.cut = span.len() > Self::CAPACITY;
    }

    fn as_bytes(&self) -> &[u8] {
        if self.cut { &[] } else { &self.bytes[..self.len as usize] }
    }
}

impl<'pa, 'ps, 'pc> Runtime<'pa, 'ps, 'pc> {
    pub fn new(
        assembly: &'pa [u8],
        strings: &'ps [&'ps str],
        charsets: &'pc [[u16; 16]],
        entrypoint: u32,
    ) -> Self {
        let mut state = RuntimeState::default();
        state.vm.registers.pc = entrypoint;
        Runtime { assembly, strings, charsets, entrypoint, state }
    }

    /// Run a `fn detect()` body against the head of a buffer and return its
    /// verdict. The bytecode halts via `return match;` (-> `true`) or
    /// `return no_match;` (-> `false`); falling off the end of the budget
    /// (line / byte cap) returns `false`.
    ///
    /// `detect_entrypoint` is the bytecode address compiled from a `fn
    /// detect()` declaration. Pass the value of [`Language::detect_entrypoint`]
    /// directly. The runtime's current entrypoint is left intact -- detect
    /// runs against its own program counter without disturbing the caller's
    /// state.
    pub fn detect(&mut self, head: &[u8], detect_entrypoint: u32) -> bool {
        // Per-invocation budgets. Tight by design: detect should fire on the
        // first signal it cares about, not chew the whole buffer.
        const MAX_LINES: usize = 80;
        const MAX_BYTES: usize = 4096;
        const MAX_INSTRUCTIONS: usize = 200_000;

        let outer_vm = mem::take(&mut self.state.vm);
        let outer_entrypoint = self.entrypoint;

        self.entrypoint = detect_entrypoint;
        self.state.vm.registers.pc = detect_entrypoint;

        let head = &head[..head.len().min(MAX_BYTES)];
        let mut verdict = false;
        let mut decided = false;
        let mut instructions = 0usize;

        'outer: for (line_idx, line) in head.split(|&b| b == b'\n').enumerate() {
            if line_idx >= MAX_LINES {
                break;
            }
            let line = line.strip_suffix(b"\r").unwrap_or(line);

            self.state.vm.registers.off = 0;
            self.state.vm.registers.hs = 0;

            loop {
                if instructions >= MAX_INSTRUCTIONS {
                    break 'outer;
                }
                instructions += 1;

                instruction_decode!(self.assembly, self.state.vm.registers.pc, {
                    Mov { dst, src } => {
                        let s = self.state.vm.registers.get(src);
                        self.state.vm.registers.set(dst, s);
                    }
                    Add { dst, src } => {
                        let d = self.state.vm.registers.get(dst);
                        let s = self.state.vm.registers.get(src);
                        self.state.vm.registers.set(dst, d.saturating_add(s));
                    }
                    Sub { dst, src } => {
                        let d = self.state.vm.registers.get(dst);
                        let s = self.state.vm.registers.get(src);
                        self.state.vm.registers.set(dst, d.saturating_sub(s));
                    }
                    MovImm { dst, imm } => {
                        self.state.vm.registers.set(dst, imm);
                    }
                    AddImm { dst, imm } => {
                        let d = self.state.vm.registers.get(dst);
                        self.state.vm.registers.set(dst, d.saturating_add(imm));
                    }
                    SubImm { dst, imm } => {
                        let d = self.state.vm.registers.get(dst);
                        self.state.vm.registers.set(dst, d.saturating_sub(imm));
                    }

                    Call { tgt } => {
                        self.state.vm.registers.save_registers(&mut self.state.vm.stack);
                        self.state.vm.registers.pc = tgt;
                    }
                    Return => {
                        if !self.state.vm.registers.load_registers(&mut self.state.vm.stack) {
                            // Empty stack on Return: detector reached the end of
                            // its body without committing to a verdict. Treat as
                            // "keep scanning subsequent lines".
                            self.state.vm.registers = Registers { pc: detect_entrypoint, ..Default::default() };
                            break;
                        }
                    }

                    JumpEQ { lhs, rhs, tgt } => {
                        if self.state.vm.registers.get(lhs) == self.state.vm.registers.get(rhs) {
                            self.state.vm.registers.pc = tgt;
                        }
                    }
                    JumpNE { lhs, rhs, tgt } => {
                        if self.state.vm.registers.get(lhs) != self.state.vm.registers.get(rhs) {
                            self.state.vm.registers.pc = tgt;
                        }
                    }
                    JumpLT { lhs, rhs, tgt } => {
                        if self.state.vm.registers.get(lhs) < self.state.vm.registers.get(rhs) {
                            self.state.vm.registers.pc = tgt;
                        }
                    }
                    JumpLE { lhs, rhs, tgt } => {
                        if self.state.vm.registers.get(lhs) <= self.state.vm.registers.get(rhs) {
                            self.state.vm.registers.pc = tgt;
                        }
                    }
                    JumpGT { lhs, rhs, tgt } => {
                        if self.state.vm.registers.get(lhs) > self.state.vm.registers.get(rhs) {
                            self.state.vm.registers.pc = tgt;
                        }
                    }
                    JumpGE { lhs, rhs, tgt } => {
                        if self.state.vm.registers.get(lhs) >= self.state.vm.registers.get(rhs) {
                            self.state.vm.registers.pc = tgt;
                        }
                    }

                    JumpIfEndOfLine { tgt } => {
                        if (self.state.vm.registers.off as usize) >= line.len() {
                            self.state.vm.registers.pc = tgt;
                        }
                    }

                    JumpIfMatchCharset { idx, min, max, tgt } => {
                        let off = self.state.vm.registers.off as usize;
                        let cs = &self.charsets[idx as usize];
                        let min = min as usize;
                        let max = max as usize;

                        if let Some(off) = Self::charset_gobble(line, off, cs, min, max) {
                            self.state.vm.registers.off = off as u32;
                            self.state.vm.registers.pc = tgt;
                        }
                    }
                    JumpIfMatchPrefix { idx, tgt } => {
                        let off = self.state.vm.registers.off as usize;
                        let str = self.strings[idx as usize].as_bytes();

                        if Self::inlined_memcmp(line, off, str) {
                            self.state.vm.registers.off = (off + str.len()) as u32;
                            self.state.vm.registers.pc = tgt;
                        }
                    }
                    JumpIfMatchPrefixInsensitive { idx, tgt } => {
                        let off = self.state.vm.registers.off as usize;
                        let str = self.strings[idx as usize].as_bytes();

                        if Self::inlined_memicmp(line, off, str) {
                            self.state.vm.registers.off = (off + str.len()) as u32;
                            self.state.vm.registers.pc = tgt;
                        }
                    }

                    FlushHighlight { kind } => {
                        // Detectors don't emit highlights. Treat as a no-op so
                        // a stray `yield` in a detect() body doesn't trip the
                        // runtime, though the frontend forbids it.
                        let _ = kind;
                        self.state.vm.registers.hs = self.state.vm.registers.off;
                    }
                    AwaitInput => {
                        let off = self.state.vm.registers.off as usize;
                        if off >= line.len() {
                            break;
                        }
                    }
                    Halt { result } => {
                        verdict = result != 0;
                        decided = true;
                        break 'outer;
                    }
                    SaveSpan { start, end } => {
                        let s = self.state.vm.registers.get(start);
                        let e = self.state.vm.registers.get(end);
                        self.state.vm.saved.set(line, s, e);
                    }
                    JumpIfMatchSaved { tgt } => {
                        let off = self.state.vm.registers.off as usize;
                        let n = self.state.vm.saved.as_bytes().len();
                        if n != 0 && Self::inlined_memcmp(line, off, self.state.vm.saved.as_bytes()) {
                            self.state.vm.registers.off = (off + n) as u32;
                            self.state.vm.registers.pc = tgt;
                        }
                    }
                    JumpIfMatchPrefixBounded { idx, tgt } => {
                        let off = self.state.vm.registers.off as usize;
                        let str = self.strings[idx as usize].as_bytes();

                        if Self::inlined_memcmp(line, off, str) && !Self::word_byte_at(line, off + str.len()) {
                            self.state.vm.registers.off = (off + str.len()) as u32;
                            self.state.vm.registers.pc = tgt;
                        }
                    }
                    JumpIfMatchPrefixInsensitiveBounded { idx, tgt } => {
                        let off = self.state.vm.registers.off as usize;
                        let str = self.strings[idx as usize].as_bytes();

                        if Self::inlined_memicmp(line, off, str) && !Self::word_byte_at(line, off + str.len()) {
                            self.state.vm.registers.off = (off + str.len()) as u32;
                            self.state.vm.registers.pc = tgt;
                        }
                    }

                    _ => unreachable!(),
                });
            }
        }

        self.state.vm = outer_vm;
        self.entrypoint = outer_entrypoint;

        if decided { verdict } else { false }
    }

    pub fn snapshot(&self) -> RuntimeState {
        self.state.clone()
    }

    pub fn restore(&mut self, state: &RuntimeState) {
        self.state = state.clone();
    }

    /// Set the current line number, readable from the DSL as `ln`.
    ///
    /// Callers that care about position-sensitive constructs (e.g. yaml
    /// frontmatter, which only opens on line 1) set this before each
    /// `parse_next_line`. The register holding it survives the per-line
    /// `off`/`hs` reset but is cleared on a top-level `Return`, so it must
    /// be set again for every line. Callers that don't set it leave `ln` at
    /// 0, which simply never matches a 1-based line guard.
    pub fn set_line_number(&mut self, line_number: u32) {
        self.state.vm.registers.set(Register::LINE_NUMBER, line_number);
    }

    /// Where the last parsed line left the conflict state.
    pub fn conflict_region(&self) -> ConflictTag {
        self.state.conflict.region()
    }

    /// Account for a line the caller will not hand to the vm (too long to
    /// highlight, say): a marker line still moves the conflict state, and
    /// any other line keeps its region.
    pub fn skip_line(&mut self, line: &[u8]) -> ConflictTag {
        if self.state.conflict.step(line, &mut self.state.vm) {
            ConflictTag::Marker
        } else {
            self.state.conflict.region()
        }
    }

    /// Parse a single line and return highlight spans.
    ///
    /// A merge-conflict marker line never reaches the bytecode: it becomes
    /// one [`kind::CONFLICT_MARKER`] span and forks or restores the vm state
    /// (see [`crate::conflict`]). Otherwise, executes bytecode until the line
    /// is fully consumed or a `Return` resets the VM. The returned spans
    /// partition the line into highlighted regions.
    ///
    /// # Returns
    /// The spans always contain at least two entries: one at offset 0 and
    /// one at `line.len()` as a sentinel. Starts never decrease, so consumers
    /// may slice `[start, next.start)` unchecked.
    pub fn parse_next_line<'a, T: PartialEq + TryFrom<u32>>(
        &mut self,
        arena: &'a Arena,
        line: &[u8],
    ) -> ParsedLine<'a, T> {
        let mut res: BVec<'a, Highlight<T>> = BVec::empty();

        if self.state.conflict.step(line, &mut self.state.vm) {
            let kind =
                T::try_from(kind::CONFLICT_MARKER).unwrap_or_else(|_| unsafe { mem::zeroed() });
            res.push(arena, Highlight { start: 0, kind });
            res.push(arena, Highlight { start: line.len(), kind: unsafe { mem::zeroed() } });
            return ParsedLine { spans: res, conflict: ConflictTag::Marker };
        }

        self.state.vm.registers.off = 0;
        self.state.vm.registers.hs = 0;

        // Every reset that abandons a line must drop its call frames too, or
        // the next top-level return pops a stale frame and jumps into the
        // middle of whatever was abandoned.
        stdext::sanity_check!(
            runtime_call_frames_whole,
            self.state.vm.stack.len().is_multiple_of(Registers::FRAME),
            "stack len {} pc {}",
            self.state.vm.stack.len(),
            self.state.vm.registers.pc
        );

        // By default, any line starts with HighlightKind::Other.
        // If the DSL yields anything, this will be overwritten.
        res.push(arena, Highlight { start: 0, kind: unsafe { mem::zeroed() } });

        // A loop whose guard can never match on this line would spin forever.
        // Generous: the busiest bundled fixture line needs about 4k.
        let budget = 256u64.saturating_mul(line.len() as u64).saturating_add(8192);
        let mut instructions = 0u64;

        loop {
            instructions += 1;
            if instructions > budget {
                stdext::sanity_check!(
                    runtime_line_within_instruction_budget,
                    false,
                    "pc={} off={} line_len={} budget={budget}",
                    self.state.vm.registers.pc,
                    self.state.vm.registers.off,
                    line.len()
                );
                self.state.vm.reset_to(self.entrypoint);
                break;
            }

            instruction_decode!(self.assembly, self.state.vm.registers.pc, {
                Mov { dst, src } => {
                    let s = self.state.vm.registers.get(src);
                    self.state.vm.registers.set(dst, s);
                }
                Add { dst, src } => {
                    let d = self.state.vm.registers.get(dst);
                    let s = self.state.vm.registers.get(src);
                    self.state.vm.registers.set(dst, d.saturating_add(s));
                }
                Sub { dst, src } => {
                    let d = self.state.vm.registers.get(dst);
                    let s = self.state.vm.registers.get(src);
                    self.state.vm.registers.set(dst, d.saturating_sub(s));
                }
                MovImm { dst, imm } => {
                    self.state.vm.registers.set(dst, imm);
                }
                AddImm { dst, imm } => {
                    let d = self.state.vm.registers.get(dst);
                    self.state.vm.registers.set(dst, d.saturating_add(imm));
                }
                SubImm { dst, imm } => {
                    let d = self.state.vm.registers.get(dst);
                    self.state.vm.registers.set(dst, d.saturating_sub(imm));
                }

                Call { tgt } => {
                    // PC already points to the next instruction (= return address)
                    self.state.vm.registers.save_registers(&mut self.state.vm.stack);
                    self.state.vm.registers.pc = tgt;
                }
                Return => {
                    if !self.state.vm.registers.load_registers(&mut self.state.vm.stack) {
                        self.state.vm.reset_to(self.entrypoint);
                        break;
                    }
                }

                JumpEQ { lhs, rhs, tgt } => {
                    if self.state.vm.registers.get(lhs) == self.state.vm.registers.get(rhs) {
                        self.state.vm.registers.pc = tgt;
                    }
                }
                JumpNE { lhs, rhs, tgt } => {
                    if self.state.vm.registers.get(lhs) != self.state.vm.registers.get(rhs) {
                        self.state.vm.registers.pc = tgt;
                    }
                }
                JumpLT { lhs, rhs, tgt } => {
                    if self.state.vm.registers.get(lhs) < self.state.vm.registers.get(rhs) {
                        self.state.vm.registers.pc = tgt;
                    }
                }
                JumpLE { lhs, rhs, tgt } => {
                    if self.state.vm.registers.get(lhs) <= self.state.vm.registers.get(rhs) {
                        self.state.vm.registers.pc = tgt;
                    }
                }
                JumpGT { lhs, rhs, tgt } => {
                    if self.state.vm.registers.get(lhs) > self.state.vm.registers.get(rhs) {
                        self.state.vm.registers.pc = tgt;
                    }
                }
                JumpGE { lhs, rhs, tgt } => {
                    if self.state.vm.registers.get(lhs) >= self.state.vm.registers.get(rhs) {
                        self.state.vm.registers.pc = tgt;
                    }
                }

                JumpIfEndOfLine { tgt } => {
                    if (self.state.vm.registers.off as usize) >= line.len() {
                        self.state.vm.registers.pc = tgt;
                    }
                }

                JumpIfMatchCharset { idx, min, max, tgt } => {
                    let off = self.state.vm.registers.off as usize;
                    let cs = &self.charsets[idx as usize];
                    let min = min as usize;
                    let max = max as usize;

                    if let Some(off) = Self::charset_gobble(line, off, cs, min, max) {
                        self.state.vm.registers.off = off as u32;
                        self.state.vm.registers.pc = tgt;
                    }
                }
                JumpIfMatchPrefix { idx, tgt } => {
                    let off = self.state.vm.registers.off as usize;
                    let str = self.strings[idx as usize].as_bytes();

                    if Self::inlined_memcmp(line, off, str) {
                        self.state.vm.registers.off = (off + str.len()) as u32;
                        self.state.vm.registers.pc = tgt;
                    }
                }
                JumpIfMatchPrefixInsensitive { idx, tgt } => {
                    let off = self.state.vm.registers.off as usize;
                    let str = self.strings[idx as usize].as_bytes();

                    if Self::inlined_memicmp(line, off, str) {
                        self.state.vm.registers.off = (off + str.len()) as u32;
                        self.state.vm.registers.pc = tgt;
                    }
                }

                FlushHighlight { kind } => {
                    let kind = self.state.vm.registers.get(kind);
                    let kind = unsafe { kind.try_into().unwrap_unchecked() };
                    let start = (self.state.vm.registers.hs as usize).min(line.len());

                    // `hs` only ever moves forward, so spans come out ordered
                    // and consumers can treat them as tiling the line. A
                    // backwards start means the definition or a compiler pass
                    // rewound it, and the only symptom downstream is text
                    // painted in the wrong colour.
                    stdext::sanity_check!(
                        runtime_highlight_starts_monotonic,
                        res.last().is_none_or(|last| start >= last.start),
                        "start={start} follows {} (line len {})",
                        res.last().map_or(0, |last| last.start),
                        line.len()
                    );

                    let start = res.last().map_or(start, |last| start.max(last.start));
                    if let Some(last) = res.last_mut()
                        && (last.start == start || last.kind == kind)
                    {
                        last.kind = kind;
                    } else {
                        res.push(arena, Highlight { start, kind });
                    }

                    self.state.vm.registers.hs = self.state.vm.registers.off;
                }
                AwaitInput => {
                    let off = self.state.vm.registers.off as usize;
                    if off >= line.len() {
                        break;
                    }
                }
                Halt { result } => {
                    let _ = result;
                    // Halt is only meaningful when driven by `detect()`. If a
                    // language definition emits one inside a highlighter
                    // entrypoint, treat it as a soft reset to the entrypoint
                    // (mirrors empty-stack Return), preventing runaway loops.
                    self.state.vm.reset_to(self.entrypoint);
                    break;
                }
                SaveSpan { start, end } => {
                    let s = self.state.vm.registers.get(start);
                    let e = self.state.vm.registers.get(end);
                    self.state.vm.saved.set(line, s, e);
                }
                JumpIfMatchSaved { tgt } => {
                    let off = self.state.vm.registers.off as usize;
                    let n = self.state.vm.saved.as_bytes().len();
                    if n != 0 && Self::inlined_memcmp(line, off, self.state.vm.saved.as_bytes()) {
                        self.state.vm.registers.off = (off + n) as u32;
                        self.state.vm.registers.pc = tgt;
                    }
                }
                JumpIfMatchPrefixBounded { idx, tgt } => {
                    let off = self.state.vm.registers.off as usize;
                    let str = self.strings[idx as usize].as_bytes();

                    if Self::inlined_memcmp(line, off, str) && !Self::word_byte_at(line, off + str.len()) {
                        self.state.vm.registers.off = (off + str.len()) as u32;
                        self.state.vm.registers.pc = tgt;
                    }
                }
                JumpIfMatchPrefixInsensitiveBounded { idx, tgt } => {
                    let off = self.state.vm.registers.off as usize;
                    let str = self.strings[idx as usize].as_bytes();

                    if Self::inlined_memicmp(line, off, str) && !Self::word_byte_at(line, off + str.len()) {
                        self.state.vm.registers.off = (off + str.len()) as u32;
                        self.state.vm.registers.pc = tgt;
                    }
                }

                _ => unreachable!(),
            });
        }

        // Ensure that there's a past-the-end highlight.
        if res.len() < 2 || res.last().is_none_or(|last| last.start < line.len()) {
            res.push(arena, Highlight { start: line.len(), kind: unsafe { mem::zeroed() } });
        }

        ParsedLine { spans: res, conflict: self.state.conflict.region() }
    }

    // TODO: http://0x80.pl/notesen/2018-10-18-simd-byte-lookup.html#alternative-implementation
    #[inline]
    fn charset_gobble(
        haystack: &[u8],
        off: usize,
        cs: &[u16; 16],
        min: usize,
        max: usize,
    ) -> Option<usize> {
        let mut i = 0usize;
        while i < max {
            let idx = off + i;
            if idx >= haystack.len() || !Self::in_set(cs, haystack[idx]) {
                break;
            }
            i += 1;
        }
        if i >= min { Some(off + i) } else { None }
    }

    /// A mini-memcmp implementation for short needles.
    /// Compares the `haystack` at `off` with the `needle`.
    #[inline]
    fn inlined_memcmp(haystack: &[u8], off: usize, needle: &[u8]) -> bool {
        unsafe {
            if off >= haystack.len() || haystack.len() - off < needle.len() {
                return false;
            }

            let a = haystack.as_ptr().add(off);
            let b = needle.as_ptr();
            let mut i = 0;

            while i < needle.len() {
                debug_assert!(off + i < haystack.len());
                let a = *a.add(i);
                let b = *b.add(i);
                i += 1;
                if a != b {
                    return false;
                }
            }

            true
        }
    }

    /// Like `inlined_memcmp`, but case-insensitive.
    #[inline]
    fn inlined_memicmp(haystack: &[u8], off: usize, needle: &[u8]) -> bool {
        unsafe {
            if off >= haystack.len() || haystack.len() - off < needle.len() {
                return false;
            }

            debug_assert!(off.checked_add(needle.len()).is_some_and(|end| end <= haystack.len()));

            let a = haystack.as_ptr().add(off);
            let b = needle.as_ptr();
            let mut i = 0;

            while i < needle.len() {
                debug_assert!(off + i < haystack.len());
                // str in PrefixInsensitive(str) is expected to be lowercase, printable ASCII.
                let a = a.add(i).read().to_ascii_lowercase();
                let b = b.add(i).read();
                i += 1;
                if a != b {
                    return false;
                }
            }

            true
        }
    }

    /// The byte after a bounded prefix must not be one of these: the regex
    /// compiler's `\w`, with the UTF-8 leading bytes it includes so a
    /// multi-byte identifier is not split.
    #[inline]
    fn word_byte_at(haystack: &[u8], at: usize) -> bool {
        haystack.get(at).is_some_and(|&b| is_word_byte(b))
    }

    #[inline]
    fn in_set(bitmap: &[u16; 16], byte: u8) -> bool {
        let lo_nibble = byte & 0xf;
        let hi_nibble = byte >> 4;

        let bitset = bitmap[lo_nibble as usize];
        let bitmask = 1u16 << hi_nibble;

        (bitset & bitmask) != 0
    }
}

/// `\w` as the regex compiler defines it: ASCII word bytes plus the UTF-8
/// leading bytes, so `\w+` and a bounded prefix agree on where a word ends.
pub const fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || (b >= 0xC2 && b <= 0xF4)
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Register {
    // These two registers are shared across function calls...
    InputOffset,
    HighlightStart,
    // ...and the rest is caller-saved.
    ProgramCounter,
    X3,
    X4,
    X5,
    X6,
    X7,
    X8,
    X9,
    X10,
    X11,
    X12,
    X13,
    X14,
    X15,
}

impl Register {
    // x3 is reserved as the line-number register (`ln` in the DSL), set per
    // line by the runtime's consumers rather than allocated to vregs. User
    // registers therefore start at x4.
    pub const LINE_NUMBER: Register = Register::X3;
    pub const FIRST_USER_REG: usize = 4; // aka x4
    pub const COUNT: usize = 16;

    #[inline(always)]
    pub fn from_usize(value: usize) -> Self {
        debug_assert!(value < Self::COUNT);
        unsafe { std::mem::transmute::<u8, Register>(value as u8) }
    }

    pub fn mnemonic(&self) -> &'static str {
        match self {
            Register::InputOffset => "off",
            Register::HighlightStart => "hs",
            Register::ProgramCounter => "pc",
            Register::X3 => "x3",
            Register::X4 => "x4",
            Register::X5 => "x5",
            Register::X6 => "x6",
            Register::X7 => "x7",
            Register::X8 => "x8",
            Register::X9 => "x9",
            Register::X10 => "x10",
            Register::X11 => "x11",
            Register::X12 => "x12",
            Register::X13 => "x13",
            Register::X14 => "x14",
            Register::X15 => "x15",
        }
    }
}

impl fmt::Display for Register {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.mnemonic())
    }
}

#[repr(C)]
#[derive(Default, Clone, Copy, PartialEq, Eq)]
pub struct Registers {
    pub off: u32, // x0 = InputOffset
    pub hs: u32,  // x1 = HighlightStart
    pub pc: u32,  // x2 = ProgramCounter
    pub x3: u32,
    pub x4: u32,
    pub x5: u32,
    pub x6: u32,
    pub x7: u32,
    pub x8: u32,
    pub x9: u32,
    pub x10: u32,
    pub x11: u32,
    pub x12: u32,
    pub x13: u32,
    pub x14: u32,
    pub x15: u32,
}

impl Registers {
    /// Words a call frame occupies on the stack: pc and the caller-saved
    /// registers x3..x15.
    const FRAME: usize = 14;

    /// The registers that carry over between lines: pc and the user
    /// registers. off and hs restart every line; x3 (ln) is set by the
    /// caller.
    fn cross_line(&self) -> [u32; 13] {
        [
            self.pc, self.x4, self.x5, self.x6, self.x7, self.x8, self.x9, self.x10, self.x11,
            self.x12, self.x13, self.x14, self.x15,
        ]
    }

    #[inline(always)]
    pub fn get(&self, reg: Register) -> u32 {
        debug_assert!((reg as usize) < Register::COUNT);
        unsafe { self.as_ptr().add(reg as usize).read() }
    }

    #[inline(always)]
    pub fn set(&mut self, reg: Register, val: u32) {
        debug_assert!((reg as usize) < Register::COUNT);
        unsafe { self.as_mut_ptr().add(reg as usize).write(val) }
    }

    #[inline(always)]
    fn save_registers(&self, vec: &mut Vec<u32>) {
        const _: () = assert!(2 + Registers::FRAME <= Register::COUNT);
        unsafe {
            vec.extend_from_slice(std::slice::from_raw_parts(self.as_ptr().add(2), Self::FRAME))
        };
    }

    #[inline(always)]
    fn load_registers(&mut self, vec: &mut Vec<u32>) -> bool {
        unsafe {
            if vec.len() < Self::FRAME {
                return false;
            }

            let src = vec.as_ptr().add(vec.len() - Self::FRAME);
            let dst = self.as_mut_ptr().add(2);
            std::ptr::copy_nonoverlapping(src, dst, Self::FRAME);
            vec.truncate(vec.len() - Self::FRAME);
            true
        }
    }

    #[inline(always)]
    unsafe fn as_ptr(&self) -> *const u32 {
        self as *const _ as *const u32
    }

    #[inline(always)]
    unsafe fn as_mut_ptr(&mut self) -> *mut u32 {
        self as *mut _ as *mut u32
    }
}

#[repr(u8)]
#[derive(Debug, Clone, Copy)]
pub enum Instruction {
    // NOTE: This allows for jumps by manipulating Register::ProgramCounter.
    Mov { dst: Register, src: Register },
    Add { dst: Register, src: Register },
    Sub { dst: Register, src: Register },
    MovImm { dst: Register, imm: u32 },
    AddImm { dst: Register, imm: u32 },
    SubImm { dst: Register, imm: u32 },

    Call { tgt: u32 },
    Return,

    JumpEQ { lhs: Register, rhs: Register, tgt: u32 }, // ==
    JumpNE { lhs: Register, rhs: Register, tgt: u32 }, // !=
    JumpLT { lhs: Register, rhs: Register, tgt: u32 }, // <
    JumpLE { lhs: Register, rhs: Register, tgt: u32 }, // <=
    JumpGT { lhs: Register, rhs: Register, tgt: u32 }, // >
    JumpGE { lhs: Register, rhs: Register, tgt: u32 }, // >=

    // Jumps to `tgt` if we're at the end of the line.
    JumpIfEndOfLine { tgt: u32 },

    // Jumps to `tgt` if the test succeeds.
    // `idx` specifies the charset/string to use.
    JumpIfMatchCharset { idx: u32, min: u32, max: u32, tgt: u32 },
    JumpIfMatchPrefix { idx: u32, tgt: u32 },
    JumpIfMatchPrefixInsensitive { idx: u32, tgt: u32 },

    // Flushes the current HighlightKind to the output.
    FlushHighlight { kind: Register },

    // Awaits more input to be available.
    AwaitInput,

    // Terminates the current bytecode run and signals a binary verdict.
    // Only used by `fn detect()` bodies. The runtime's `detect()` driver
    // breaks its line loop on this opcode and surfaces `result` as a bool.
    Halt { result: u32 },

    // Remembers `line[start..end]` across lines. See [`SavedSpan`].
    SaveSpan { start: Register, end: Register },

    // Jumps to `tgt` if the remembered span is a non-empty prefix of the
    // input at `off`, consuming it.
    JumpIfMatchSaved { tgt: u32 },

    // The prefix jumps with `\>` folded in: the match only counts when the
    // byte after it is not a word byte, so a keyword is not taken for the
    // start of a longer identifier. One instruction per alternative of a
    // keyword list rather than three.
    JumpIfMatchPrefixBounded { idx: u32, tgt: u32 },
    JumpIfMatchPrefixInsensitiveBounded { idx: u32, tgt: u32 },
}

macro_rules! instruction_decode {
    ($assembly:expr, $pc:expr, {
        Mov { $mov_dst:ident, $mov_src:ident } => $mov_handler:block
        Add { $add_dst:ident, $add_src:ident } => $add_handler:block
        Sub { $sub_dst:ident, $sub_src:ident } => $sub_handler:block
        MovImm { $movi_dst:ident, $movi_imm:ident } => $movi_handler:block
        AddImm { $addi_dst:ident, $addi_imm:ident } => $addi_handler:block
        SubImm { $subi_dst:ident, $subi_imm:ident } => $subi_handler:block

        Call { $call_tgt:ident } => $call_handler:block
        Return => $ret_handler:block

        JumpEQ { $jeq_lhs:ident, $jeq_rhs:ident, $jeq_tgt:ident } => $jeq_handler:block
        JumpNE { $jne_lhs:ident, $jne_rhs:ident, $jne_tgt:ident } => $jne_handler:block
        JumpLT { $jlt_lhs:ident, $jlt_rhs:ident, $jlt_tgt:ident } => $jlt_handler:block
        JumpLE { $jle_lhs:ident, $jle_rhs:ident, $jle_tgt:ident } => $jle_handler:block
        JumpGT { $jgt_lhs:ident, $jgt_rhs:ident, $jgt_tgt:ident } => $jgt_handler:block
        JumpGE { $jge_lhs:ident, $jge_rhs:ident, $jge_tgt:ident } => $jge_handler:block

        JumpIfEndOfLine { $jeol_tgt:ident } => $jeol_handler:block

        JumpIfMatchCharset { $jc_idx:ident, $jc_min:ident, $jc_max:ident, $jc_tgt:ident } => $jc_handler:block
        JumpIfMatchPrefix { $jp_idx:ident, $jp_tgt:ident } => $jp_handler:block
        JumpIfMatchPrefixInsensitive { $jpi_idx:ident, $jpi_tgt:ident } => $jpi_handler:block

        FlushHighlight { $flush_kind:ident } => $flush_handler:block
        AwaitInput => $await_handler:block
        Halt { $halt_result:ident } => $halt_handler:block

        SaveSpan { $ss_start:ident, $ss_end:ident } => $ss_handler:block
        JumpIfMatchSaved { $jms_tgt:ident } => $jms_handler:block
        JumpIfMatchPrefixBounded { $jpb_idx:ident, $jpb_tgt:ident } => $jpb_handler:block
        JumpIfMatchPrefixInsensitiveBounded { $jpib_idx:ident, $jpib_tgt:ident } => $jpib_handler:block

        _ => $bad_opcode:expr $(,)?
    }) => {{
        #[inline(always)]
        fn dec_reg_single(bytes: &[u8], off: usize) -> Register {
            debug_assert!(off < bytes.len());
            let b = unsafe { *bytes.as_ptr().add(off) as usize };
            Register::from_usize(b & 0xf)
        }

        #[inline(always)]
        fn dec_reg_pair(bytes: &[u8], off: usize) -> (Register, Register) {
            debug_assert!(off < bytes.len());
            let b = unsafe { *bytes.as_ptr().add(off) as usize };
            let dst = Register::from_usize(b & 0xf);
            let src = Register::from_usize(b >> 4);
            (dst, src)
        }

        #[inline(always)]
        fn dec_u32(bytes: &[u8], off: usize) -> u32 {
            debug_assert!(off + 4 <= bytes.len());
            unsafe { (bytes.as_ptr().add(off) as *const u32).read_unaligned() }
        }

        let __asm: &[u8] = $assembly;
        let __off = $pc as usize;

        // The use of unsafe code above boosts performance by about 10%. We rely on the code
        // generator to emit invalid 0xff opcodes at the end of the instruction stream as padding.
        // This way we can read past-the-end, even if the last non-0xff instruction is truncated.
        let __opcode = __asm[__off];

        match __opcode {
            0 => {
                // Mov
                $pc += 2;
                let ($mov_dst, $mov_src) = dec_reg_pair(__asm, __off + 1);
                $mov_handler
            }
            1 => {
                // Add
                $pc += 2;
                let ($add_dst, $add_src) = dec_reg_pair(__asm, __off + 1);
                $add_handler
            }
            2 => {
                // Sub
                $pc += 2;
                let ($sub_dst, $sub_src) = dec_reg_pair(__asm, __off + 1);
                $sub_handler
            }
            3 => {
                // MovImm
                $pc += 6;
                let $movi_dst = dec_reg_single(__asm, __off + 1);
                let $movi_imm = dec_u32(__asm, __off + 2);
                $movi_handler
            }
            4 => {
                // AddImm
                $pc += 6;
                let $addi_dst = dec_reg_single(__asm, __off + 1);
                let $addi_imm = dec_u32(__asm, __off + 2);
                $addi_handler
            }
            5 => {
                // SubImm
                $pc += 6;
                let $subi_dst = dec_reg_single(__asm, __off + 1);
                let $subi_imm = dec_u32(__asm, __off + 2);
                $subi_handler
            }

            6 => {
                // Call
                $pc += 5;
                let $call_tgt = dec_u32(__asm, __off + 1);
                $call_handler
            }
            7 => {
                // Return
                $pc += 1;
                $ret_handler
            }

            8 => {
                // JumpEQ
                $pc += 6;
                let ($jeq_lhs, $jeq_rhs) = dec_reg_pair(__asm, __off + 1);
                let $jeq_tgt = dec_u32(__asm, __off + 2);
                $jeq_handler
            }
            9 => {
                // JumpNE
                $pc += 6;
                let ($jne_lhs, $jne_rhs) = dec_reg_pair(__asm, __off + 1);
                let $jne_tgt = dec_u32(__asm, __off + 2);
                $jne_handler
            }
            10 => {
                // JumpLT
                $pc += 6;
                let ($jlt_lhs, $jlt_rhs) = dec_reg_pair(__asm, __off + 1);
                let $jlt_tgt = dec_u32(__asm, __off + 2);
                $jlt_handler
            }
            11 => {
                // JumpLE
                $pc += 6;
                let ($jle_lhs, $jle_rhs) = dec_reg_pair(__asm, __off + 1);
                let $jle_tgt = dec_u32(__asm, __off + 2);
                $jle_handler
            }
            12 => {
                // JumpGT
                $pc += 6;
                let ($jgt_lhs, $jgt_rhs) = dec_reg_pair(__asm, __off + 1);
                let $jgt_tgt = dec_u32(__asm, __off + 2);
                $jgt_handler
            }
            13 => {
                // JumpGE
                $pc += 6;
                let ($jge_lhs, $jge_rhs) = dec_reg_pair(__asm, __off + 1);
                let $jge_tgt = dec_u32(__asm, __off + 2);
                $jge_handler
            }

            14 => {
                // JumpIfEndOfLine
                $pc += 5;
                let $jeol_tgt = dec_u32(__asm, __off + 1);
                $jeol_handler
            }

            15 => {
                // JumpIfMatchCharset
                $pc += 17;
                let $jc_idx = dec_u32(__asm, __off + 1);
                let $jc_min = dec_u32(__asm, __off + 5);
                let $jc_max = dec_u32(__asm, __off + 9);
                let $jc_tgt = dec_u32(__asm, __off + 13);
                $jc_handler
            }
            16 => {
                // JumpIfMatchPrefix
                $pc += 9;
                let $jp_idx = dec_u32(__asm, __off + 1);
                let $jp_tgt = dec_u32(__asm, __off + 5);
                $jp_handler
            }
            17 => {
                // JumpIfMatchPrefixInsensitive
                $pc += 9;
                let $jpi_idx = dec_u32(__asm, __off + 1);
                let $jpi_tgt = dec_u32(__asm, __off + 5);
                $jpi_handler
            }

            18 => {
                // FlushHighlight
                $pc += 2;
                let $flush_kind = dec_reg_single(__asm, __off + 1);
                $flush_handler
            }
            19 => {
                // AwaitInput
                $pc += 1;
                $await_handler
            }
            20 => {
                // Halt
                $pc += 5;
                let $halt_result = dec_u32(__asm, __off + 1);
                $halt_handler
            }
            21 => {
                // SaveSpan
                $pc += 2;
                let ($ss_start, $ss_end) = dec_reg_pair(__asm, __off + 1);
                $ss_handler
            }
            22 => {
                // JumpIfMatchSaved
                $pc += 5;
                let $jms_tgt = dec_u32(__asm, __off + 1);
                $jms_handler
            }
            23 => {
                // JumpIfMatchPrefixBounded
                $pc += 9;
                let $jpb_idx = dec_u32(__asm, __off + 1);
                let $jpb_tgt = dec_u32(__asm, __off + 5);
                $jpb_handler
            }
            24 => {
                // JumpIfMatchPrefixInsensitiveBounded
                $pc += 9;
                let $jpib_idx = dec_u32(__asm, __off + 1);
                let $jpib_tgt = dec_u32(__asm, __off + 5);
                $jpib_handler
            }

            _ => $bad_opcode,
        }
    }};
}

use instruction_decode;

impl Instruction {
    // JumpIfMatchCharset, etc., are 1 byte opcode + 4 u32 parameters.
    pub const MAX_ENCODED_SIZE: usize = 1 + 4 * 4;

    pub fn address_offset(&self) -> Option<usize> {
        match *self {
            Instruction::MovImm { .. }
            | Instruction::AddImm { .. }
            | Instruction::SubImm { .. } => Some(1 + 1), // opcode + dst

            Instruction::Call { .. } => Some(1), // opcode

            Instruction::JumpEQ { .. }
            | Instruction::JumpNE { .. }
            | Instruction::JumpLT { .. }
            | Instruction::JumpLE { .. }
            | Instruction::JumpGT { .. }
            | Instruction::JumpGE { .. } => Some(1 + 1), // opcode + lhs/rhs pair

            Instruction::JumpIfEndOfLine { .. } => Some(1), // opcode

            Instruction::JumpIfMatchCharset { .. } => Some(1 + 3 * 4), // opcode + idx + min + max
            Instruction::JumpIfMatchPrefix { .. }
            | Instruction::JumpIfMatchPrefixInsensitive { .. }
            | Instruction::JumpIfMatchPrefixBounded { .. }
            | Instruction::JumpIfMatchPrefixInsensitiveBounded { .. } => Some(1 + 4), // opcode + idx

            Instruction::JumpIfMatchSaved { .. } => Some(1), // opcode

            _ => None,
        }
    }

    #[allow(clippy::identity_op)]
    pub fn encode<'a>(&self, arena: &'a Arena) -> BVec<'a, u8> {
        fn enc_reg_pair(lo: Register, hi: Register) -> u8 {
            ((hi as u8) << 4) | (lo as u8)
        }

        fn enc_reg_single(lo: Register) -> u8 {
            lo as u8
        }

        fn enc_u32(val: u32) -> [u8; 4] {
            val.to_le_bytes()
        }

        let mut bytes = BVec::empty();
        #[allow(clippy::missing_transmute_annotations)]
        bytes.push(arena, unsafe { std::mem::transmute(std::mem::discriminant(self)) });

        match *self {
            Instruction::Mov { dst, src }
            | Instruction::Add { dst, src }
            | Instruction::Sub { dst, src } => {
                bytes.push(arena, enc_reg_pair(dst, src));
            }
            Instruction::MovImm { dst, imm }
            | Instruction::AddImm { dst, imm }
            | Instruction::SubImm { dst, imm } => {
                bytes.push(arena, enc_reg_single(dst));
                bytes.extend_from_slice(arena, &enc_u32(imm));
            }

            Instruction::Call { tgt } => {
                bytes.extend_from_slice(arena, &enc_u32(tgt));
            }
            Instruction::Return => {}

            Instruction::JumpEQ { lhs, rhs, tgt }
            | Instruction::JumpNE { lhs, rhs, tgt }
            | Instruction::JumpLT { lhs, rhs, tgt }
            | Instruction::JumpLE { lhs, rhs, tgt }
            | Instruction::JumpGT { lhs, rhs, tgt }
            | Instruction::JumpGE { lhs, rhs, tgt } => {
                bytes.push(arena, enc_reg_pair(lhs, rhs));
                bytes.extend_from_slice(arena, &enc_u32(tgt));
            }

            Instruction::JumpIfEndOfLine { tgt } => {
                bytes.extend_from_slice(arena, &enc_u32(tgt));
            }
            Instruction::JumpIfMatchCharset { idx, min, max, tgt } => {
                bytes.extend_from_slice(arena, &enc_u32(idx));
                bytes.extend_from_slice(arena, &enc_u32(min));
                bytes.extend_from_slice(arena, &enc_u32(max));
                bytes.extend_from_slice(arena, &enc_u32(tgt));
            }
            Instruction::JumpIfMatchPrefix { idx, tgt }
            | Instruction::JumpIfMatchPrefixInsensitive { idx, tgt }
            | Instruction::JumpIfMatchPrefixBounded { idx, tgt }
            | Instruction::JumpIfMatchPrefixInsensitiveBounded { idx, tgt } => {
                bytes.extend_from_slice(arena, &enc_u32(idx));
                bytes.extend_from_slice(arena, &enc_u32(tgt));
            }

            Instruction::FlushHighlight { kind } => {
                bytes.push(arena, enc_reg_single(kind));
            }
            Instruction::AwaitInput => {}
            Instruction::Halt { result } => {
                bytes.extend_from_slice(arena, &enc_u32(result));
            }
            Instruction::SaveSpan { start, end } => {
                bytes.push(arena, enc_reg_pair(start, end));
            }
            Instruction::JumpIfMatchSaved { tgt } => {
                bytes.extend_from_slice(arena, &enc_u32(tgt));
            }
        }

        bytes
    }

    pub fn decode(bytes: &[u8]) -> (Option<Self>, usize) {
        let mut pc = 0;
        let instr = instruction_decode!(bytes, pc, {
            Mov { dst, src } => {
                Instruction::Mov { dst, src }
            }
            Add { dst, src } => {
                Instruction::Add { dst, src }
            }
            Sub { dst, src } => {
                Instruction::Sub { dst, src }
            }
            MovImm { dst, imm } => {
                Instruction::MovImm { dst, imm }
            }
            AddImm { dst, imm } => {
                Instruction::AddImm { dst, imm }
            }
            SubImm { dst, imm } => {
                Instruction::SubImm { dst, imm }
            }
            Call { tgt } => {
                Instruction::Call { tgt }
            }
            Return => {
                Instruction::Return
            }
            JumpEQ { lhs, rhs, tgt } => {
                Instruction::JumpEQ { lhs, rhs, tgt }
            }
            JumpNE { lhs, rhs, tgt } => {
                Instruction::JumpNE { lhs, rhs, tgt }
            }
            JumpLT { lhs, rhs, tgt } => {
                Instruction::JumpLT { lhs, rhs, tgt }
            }
            JumpLE { lhs, rhs, tgt } => {
                Instruction::JumpLE { lhs, rhs, tgt }
            }
            JumpGT { lhs, rhs, tgt } => {
                Instruction::JumpGT { lhs, rhs, tgt }
            }
            JumpGE { lhs, rhs, tgt } => {
                Instruction::JumpGE { lhs, rhs, tgt }
            }
            JumpIfEndOfLine { tgt }=> {
                Instruction::JumpIfEndOfLine { tgt }
            }
            JumpIfMatchCharset { idx, min, max, tgt } => {
                Instruction::JumpIfMatchCharset { idx, min, max, tgt }
            }
            JumpIfMatchPrefix { idx, tgt } => {
                Instruction::JumpIfMatchPrefix { idx, tgt }
            }
            JumpIfMatchPrefixInsensitive { idx, tgt } => {
                Instruction::JumpIfMatchPrefixInsensitive { idx, tgt }
            }
            FlushHighlight { kind } => {
                Instruction::FlushHighlight { kind }
            }
            AwaitInput=> {
                Instruction::AwaitInput
            }
            Halt { result } => {
                Instruction::Halt { result }
            }
            SaveSpan { start, end } => {
                Instruction::SaveSpan { start, end }
            }
            JumpIfMatchSaved { tgt } => {
                Instruction::JumpIfMatchSaved { tgt }
            }
            JumpIfMatchPrefixBounded { idx, tgt } => {
                Instruction::JumpIfMatchPrefixBounded { idx, tgt }
            }
            JumpIfMatchPrefixInsensitiveBounded { idx, tgt } => {
                Instruction::JumpIfMatchPrefixInsensitiveBounded { idx, tgt }
            }
            _ => return (None, 1),
        });
        (Some(instr), pc)
    }

    pub fn mnemonic<'a>(&self, arena: &'a Arena, config: &MnemonicFormattingConfig) -> BString<'a> {
        let mut str = BString::empty();
        let _i = config.instruction_prefix;
        let i_ = config.instruction_suffix;
        let _r = config.register_prefix;
        let r_ = config.register_suffix;
        let _a = config.address_prefix;
        let a_ = config.address_suffix;
        let _n = config.numeric_prefix;
        let n_ = config.numeric_suffix;

        match *self {
            Instruction::Mov { dst, src } => {
                arena_write_fmt!(arena, str, "{_i}mov{i_}    {_r}{dst}{r_}, {_r}{src}{r_}");
            }
            Instruction::Add { dst, src } => {
                arena_write_fmt!(arena, str, "{_i}add{i_}    {_r}{dst}{r_}, {_r}{src}{r_}");
            }
            Instruction::Sub { dst, src } => {
                arena_write_fmt!(arena, str, "{_i}sub{i_}    {_r}{dst}{r_}, {_r}{src}{r_}");
            }
            Instruction::MovImm { dst, imm } => {
                if dst == Register::ProgramCounter {
                    arena_write_fmt!(arena, str, "{_i}movi{i_}   {_r}{dst}{r_}, {_a}{imm}{a_}");
                } else {
                    arena_write_fmt!(arena, str, "{_i}movi{i_}   {_r}{dst}{r_}, {_n}{imm}{n_}");
                }
            }
            Instruction::AddImm { dst, imm } => {
                arena_write_fmt!(arena, str, "{_i}addi{i_}   {_r}{dst}{r_}, {_n}{imm}{n_}");
            }
            Instruction::SubImm { dst, imm } => {
                arena_write_fmt!(arena, str, "{_i}subi{i_}   {_r}{dst}{r_}, {_n}{imm}{n_}");
            }

            Instruction::Call { tgt } => {
                arena_write_fmt!(arena, str, "{_i}call{i_}   {_a}{tgt}{a_}");
            }
            Instruction::Return => {
                arena_write_fmt!(arena, str, "{_i}ret{i_}");
            }

            Instruction::JumpEQ { lhs, rhs, tgt } => {
                arena_write_fmt!(
                    arena,
                    str,
                    "{_i}jeq{i_}    {_r}{lhs}{r_}, {_r}{rhs}{r_}, {_a}{tgt}{a_}"
                );
            }
            Instruction::JumpNE { lhs, rhs, tgt } => {
                arena_write_fmt!(
                    arena,
                    str,
                    "{_i}jne{i_}    {_r}{lhs}{r_}, {_r}{rhs}{r_}, {_a}{tgt}{a_}"
                );
            }
            Instruction::JumpLT { lhs, rhs, tgt } => {
                arena_write_fmt!(
                    arena,
                    str,
                    "{_i}jlt{i_}    {_r}{lhs}{r_}, {_r}{rhs}{r_}, {_a}{tgt}{a_}"
                );
            }
            Instruction::JumpLE { lhs, rhs, tgt } => {
                arena_write_fmt!(
                    arena,
                    str,
                    "{_i}jle{i_}    {_r}{lhs}{r_}, {_r}{rhs}{r_}, {_a}{tgt}{a_}"
                );
            }
            Instruction::JumpGT { lhs, rhs, tgt } => {
                arena_write_fmt!(
                    arena,
                    str,
                    "{_i}jgt{i_}    {_r}{lhs}{r_}, {_r}{rhs}{r_}, {_a}{tgt}{a_}"
                );
            }
            Instruction::JumpGE { lhs, rhs, tgt } => {
                arena_write_fmt!(
                    arena,
                    str,
                    "{_i}jge{i_}    {_r}{lhs}{r_}, {_r}{rhs}{r_}, {_a}{tgt}{a_}"
                );
            }

            Instruction::JumpIfEndOfLine { tgt } => {
                arena_write_fmt!(arena, str, "{_i}jeol{i_}   {_a}{tgt}{a_}");
            }
            Instruction::JumpIfMatchCharset { idx, min, max, tgt } => {
                arena_write_fmt!(
                    arena,
                    str,
                    "{_i}jc{i_}     {_n}{idx}{n_}, {_n}{min}{n_}, {_n}{max}{n_}, {_a}{tgt}{a_}"
                );
            }
            Instruction::JumpIfMatchPrefix { idx, tgt } => {
                arena_write_fmt!(arena, str, "{_i}jp{i_}     {_n}{idx}{n_}, {_a}{tgt}{a_}");
            }
            Instruction::JumpIfMatchPrefixInsensitive { idx, tgt } => {
                arena_write_fmt!(arena, str, "{_i}jpi{i_}    {_n}{idx}{n_}, {_a}{tgt}{a_}");
            }

            Instruction::FlushHighlight { kind } => {
                arena_write_fmt!(arena, str, "{_i}flush{i_}  {_r}{kind}{r_}");
            }
            Instruction::AwaitInput => {
                arena_write_fmt!(arena, str, "{_i}await{i_}");
            }
            Instruction::Halt { result } => {
                arena_write_fmt!(arena, str, "{_i}halt{i_}   {_n}{result}{n_}");
            }
            Instruction::SaveSpan { start, end } => {
                arena_write_fmt!(arena, str, "{_i}save{i_}   {_r}{start}{r_}, {_r}{end}{r_}");
            }
            Instruction::JumpIfMatchSaved { tgt } => {
                arena_write_fmt!(arena, str, "{_i}jsv{i_}    {_a}{tgt}{a_}");
            }
            Instruction::JumpIfMatchPrefixBounded { idx, tgt } => {
                arena_write_fmt!(arena, str, "{_i}jpb{i_}    {_n}{idx}{n_}, {_a}{tgt}{a_}");
            }
            Instruction::JumpIfMatchPrefixInsensitiveBounded { idx, tgt } => {
                arena_write_fmt!(arena, str, "{_i}jpib{i_}   {_n}{idx}{n_}, {_a}{tgt}{a_}");
            }
        }

        str
    }
}

#[derive(Default)]
pub struct MnemonicFormattingConfig<'a> {
    // Color used for highlighting the instruction.
    pub instruction_prefix: &'a str,
    pub instruction_suffix: &'a str,

    // Color used for highlighting a register name.
    pub register_prefix: &'a str,
    pub register_suffix: &'a str,

    // Color used for highlighting an immediate value.
    pub address_prefix: &'a str,
    pub address_suffix: &'a str,

    // Color used for highlighting an immediate value.
    pub numeric_prefix: &'a str,
    pub numeric_suffix: &'a str,
}

#[cfg(test)]
mod tests {
    use stdext::arena::scratch_arena;

    use super::*;
    use crate::compiler::{Compiler, SerializedCharset};

    /// Compile a single-definition source and run it over `lines`, returning
    /// the `(kind, text)` spans of each line.
    fn highlight(src: &str, lines: &[&str]) -> Vec<Vec<(String, String)>> {
        let _ = stdext::arena::init(16 * 1024 * 1024);

        let arena = scratch_arena(None);
        let mut compiler = Compiler::new(&arena);
        compiler.parse("test.lsh", src).unwrap();
        let assembly = compiler.assemble().unwrap();

        let charsets: Vec<SerializedCharset> =
            assembly.charsets.iter().map(|cs| cs.serialize()).collect();
        let max_id = assembly.highlight_kinds.iter().map(|hk| hk.value).max().unwrap_or(0);
        let mut kind_names: Vec<&str> = vec![""; max_id as usize + 1];
        for hk in &assembly.highlight_kinds {
            kind_names[hk.value as usize] = hk.identifier;
        }

        let mut runtime = Runtime::new(
            &assembly.instructions,
            &assembly.strings,
            &charsets,
            assembly.entrypoints[0].address as u32,
        );

        lines
            .iter()
            .map(|line| {
                let scratch = scratch_arena(Some(&arena));
                let highlights = runtime.parse_next_line::<u32>(&scratch, line.as_bytes()).spans;
                highlights
                    .windows(2)
                    .filter(|w| w[0].start != w[1].start)
                    .map(|w| {
                        let kind = kind_names.get(w[0].kind as usize).copied().unwrap_or("?");
                        (kind.to_string(), line[w[0].start..w[1].start].to_string())
                    })
                    .collect()
            })
            .collect()
    }

    /// A block comment that swallows whole lines until `*/`, and words as
    /// keywords: enough to see a construct leak (or not) across a conflict.
    const COMMENT_DEF: &str = "#[display_name = \"T\"]\n\
                               #[path = \"**/*.t\"]\n\
                               pub fn t() {\n\
                                   if /\\/\\*/ {\n\
                                       loop {\n\
                                           yield comment;\n\
                                           await input;\n\
                                           if /\\*\\// { yield comment; break; }\n\
                                           if /.*/ {}\n\
                                       }\n\
                                       return;\n\
                                   }\n\
                                   if /\\w+/ { yield keyword; }\n\
                               }\n";

    /// Like `highlight`, keeping only each line's kinds plus its conflict tag.
    fn kinds_and_tags(src: &str, lines: &[&str]) -> Vec<(Vec<String>, ConflictTag)> {
        let _ = stdext::arena::init(16 * 1024 * 1024);
        let arena = scratch_arena(None);
        let mut compiler = Compiler::new(&arena);
        compiler.parse("test.lsh", src).unwrap();
        let assembly = compiler.assemble().unwrap();
        let charsets: Vec<SerializedCharset> =
            assembly.charsets.iter().map(|cs| cs.serialize()).collect();
        let mut kind_names: Vec<&str> = vec![""; assembly.highlight_kinds.len()];
        for hk in &assembly.highlight_kinds {
            kind_names[hk.value as usize] = hk.identifier;
        }
        let entry = assembly.entrypoints[0].address as u32;
        let mut runtime = Runtime::new(&assembly.instructions, &assembly.strings, &charsets, entry);
        lines
            .iter()
            .map(|line| {
                let scratch = scratch_arena(Some(&arena));
                let parsed = runtime.parse_next_line::<u32>(&scratch, line.as_bytes());
                let kinds = parsed
                    .spans
                    .windows(2)
                    .filter(|w| w[0].start != w[1].start)
                    .map(|w| kind_names[w[0].kind as usize].to_string())
                    .collect();
                (kinds, parsed.conflict)
            })
            .collect()
    }

    fn kinds(parsed: &[(Vec<String>, ConflictTag)]) -> Vec<Vec<&str>> {
        parsed.iter().map(|(k, _)| k.iter().map(String::as_str).collect()).collect()
    }

    fn tags(parsed: &[(Vec<String>, ConflictTag)]) -> Vec<ConflictTag> {
        parsed.iter().map(|(_, t)| *t).collect()
    }

    #[test]
    fn a_construct_opened_on_one_side_does_not_leak_into_the_other() {
        use ConflictTag::*;
        let parsed = kinds_and_tags(
            COMMENT_DEF,
            &[
                "a",
                "<<<<<<< HEAD",
                "/* open",
                "||||||| base",
                "b",
                "=======",
                "c",
                ">>>>>>> t",
                "d",
            ],
        );
        let m = "markup.conflict.marker";
        assert_eq!(
            kinds(&parsed),
            [
                vec!["keyword"],
                vec![m],
                vec!["comment"],
                vec![m],
                vec!["keyword"],
                vec![m],
                vec!["keyword"],
                vec![m],
                vec!["keyword"],
            ]
        );
        assert_eq!(tags(&parsed), [None, Marker, Ours, Marker, Base, Marker, Theirs, Marker, None]);
    }

    #[test]
    fn the_text_after_a_block_continues_from_the_side_that_closed_its_constructs() {
        let ours_open = kinds_and_tags(
            COMMENT_DEF,
            &["<<<<<<< HEAD", "/* open", "=======", "x", ">>>>>>> t", "d"],
        );
        assert_eq!(kinds(&ours_open)[5], ["keyword"]);

        let theirs_open = kinds_and_tags(
            COMMENT_DEF,
            &["<<<<<<< HEAD", "x", "=======", "/* open", ">>>>>>> t", "d"],
        );
        assert_eq!(kinds(&theirs_open)[5], ["keyword"]);

        let both_open = kinds_and_tags(
            COMMENT_DEF,
            &["<<<<<<< HEAD", "/* a", "=======", "/* b", ">>>>>>> t", "d"],
        );
        assert_eq!(kinds(&both_open)[5], ["comment"]);
    }

    #[test]
    fn markers_of_another_length_inside_a_block_are_text() {
        use ConflictTag::*;
        let parsed = kinds_and_tags(
            COMMENT_DEF,
            &[
                "<<<<<<< HEAD",
                "<<<<<<<<< inner",
                "=========",
                ">>>>>>>>> inner",
                "=======",
                "c",
                ">>>>>>> t",
            ],
        );
        assert_eq!(tags(&parsed), [Marker, Ours, Ours, Ours, Marker, Theirs, Marker]);
        assert_eq!(kinds(&parsed)[1], ["other"]);
    }

    #[test]
    fn a_repeated_opener_restarts_the_block_from_the_fork() {
        use ConflictTag::*;
        // From each of the three in-block states in turn, with a construct
        // left open just before the repeat.
        let from_ours = kinds_and_tags(
            COMMENT_DEF,
            &["<<<<<<< HEAD", "/* open", "<<<<<<< HEAD", "x", "=======", "y", ">>>>>>> t"],
        );
        assert_eq!(tags(&from_ours), [Marker, Ours, Marker, Ours, Marker, Theirs, Marker]);
        assert_eq!(kinds(&from_ours)[3], ["keyword"]);

        let from_base = kinds_and_tags(
            COMMENT_DEF,
            &["<<<<<<< HEAD", "a", "||||||| b", "/* open", "<<<<<<< HEAD", "x", ">>>>>>> t"],
        );
        assert_eq!(tags(&from_base), [Marker, Ours, Marker, Base, Marker, Ours, Ours]);
        assert_eq!(kinds(&from_base)[5], ["keyword"]);

        let from_theirs = kinds_and_tags(
            COMMENT_DEF,
            &["<<<<<<< HEAD", "a", "=======", "/* open", "<<<<<<< HEAD", "x", "=======", "y"],
        );
        assert_eq!(
            tags(&from_theirs),
            [Marker, Ours, Marker, Theirs, Marker, Ours, Marker, Theirs]
        );
        assert_eq!(kinds(&from_theirs)[5], ["keyword"]);
    }

    #[test]
    fn an_unterminated_block_stays_open_and_a_stray_separator_is_text() {
        use ConflictTag::*;
        let open = kinds_and_tags(COMMENT_DEF, &["<<<<<<< HEAD", "x", "y"]);
        assert_eq!(tags(&open), [Marker, Ours, Ours]);
        assert_eq!(kinds(&open)[2], ["keyword"]);

        let stray = kinds_and_tags(COMMENT_DEF, &["=======", ">>>>>>> t", "x"]);
        assert_eq!(tags(&stray), [None, None, None]);
        assert_eq!(kinds(&stray)[0], ["other"]);
    }

    /// The editor re-highlights from cached states; a state taken inside a
    /// block must still know the fork when it is restored elsewhere.
    #[test]
    fn a_snapshot_taken_inside_a_block_carries_the_fork() {
        let _ = stdext::arena::init(16 * 1024 * 1024);
        let arena = scratch_arena(None);
        let mut compiler = Compiler::new(&arena);
        compiler.parse("test.lsh", COMMENT_DEF).unwrap();
        let assembly = compiler.assemble().unwrap();
        let charsets: Vec<SerializedCharset> =
            assembly.charsets.iter().map(|cs| cs.serialize()).collect();
        let keyword =
            assembly.highlight_kinds.iter().find(|hk| hk.identifier == "keyword").unwrap().value;
        let entry = assembly.entrypoints[0].address as u32;
        let mut runtime = Runtime::new(&assembly.instructions, &assembly.strings, &charsets, entry);

        runtime.parse_next_line::<u32>(&arena, b"<<<<<<< HEAD");
        runtime.parse_next_line::<u32>(&arena, b"/* open");
        let state = runtime.snapshot();

        let mut resumed = Runtime::new(&assembly.instructions, &assembly.strings, &charsets, entry);
        resumed.restore(&state);
        assert_eq!(resumed.conflict_region(), ConflictTag::Ours);
        let sep = resumed.parse_next_line::<u32>(&arena, b"=======");
        assert_eq!(sep.conflict, ConflictTag::Marker);
        let theirs = resumed.parse_next_line::<u32>(&arena, b"c");
        assert_eq!(theirs.conflict, ConflictTag::Theirs);
        assert_eq!(theirs.spans[0].kind, keyword);
    }

    /// The abandoned line was inside a helper call. Its frame must go with
    /// it, or the next top-level return pops the frame and resumes the loop
    /// on a line that has nothing to do with it.
    #[test]
    fn a_cut_off_line_leaves_no_call_frame_behind() {
        let src = "#[display_name = \"T\"]\n\
                   #[path = \"**/*.t\"]\n\
                   pub fn t() { if /\\[/ { until /\\]/ { helper(); } } }\n\
                   fn helper() { yield keyword; }\n";
        let (spans, _) = captured(|| highlight(src, &["[unix", "plain", "plain"]));
        let kinds = |i: usize| spans[i].iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>();
        assert_eq!(kinds(1), ["other"]);
        assert_eq!(kinds(2), ["other"]);
    }

    /// Runs `f` with sanity trips captured. The notify handler and the dedup
    /// window are process-wide, so a test that expects a check to fire must
    /// not let it leak into a parallel test's capture.
    fn captured<R>(f: impl FnOnce() -> R) -> (R, Vec<String>) {
        #[cfg(feature = "sanity")]
        {
            stdext::sanity::capture::trips(f)
        }
        #[cfg(not(feature = "sanity"))]
        {
            (f(), Vec::new())
        }
    }

    /// A loop's no-progress check compares the offset against where the
    /// iteration started. Across an `await input` that baseline belongs to the
    /// previous line, and the check used to fire on the new line and force the
    /// offset past its first character -- here, past the closing backtick.
    #[test]
    fn a_loop_that_awaits_input_keeps_the_first_column_of_the_next_line() {
        let src = "#[display_name = \"T\"]\n\
                   #[path = \"**/*.t\"]\n\
                   pub fn t() {\n\
                       if /`/ {\n\
                           loop {\n\
                               if /[^`]+/ {}\n\
                               if /`/ { yield string; break; }\n\
                               yield string;\n\
                               await input;\n\
                           }\n\
                           if /.*/ {}\n\
                           yield other;\n\
                       }\n\
                   }\n";

        let spans = highlight(src, &["`aa", "bb", "` tail"]);

        assert_eq!(spans[0], [("string".to_string(), "`aa".to_string())]);
        assert_eq!(spans[1], [("string".to_string(), "bb".to_string())]);
        assert_eq!(
            spans[2],
            [("string".to_string(), "`".to_string()), ("other".to_string(), " tail".to_string())]
        );
    }

    /// A counter bumped with `+=` inside a loop is read as well as written,
    /// so it must stay live across the back-edge. The allocator used to see
    /// only the write, decide the register was free at the loop head, and
    /// hand it to the temporary every `yield` goes through -- the counter
    /// then read whatever kind was flushed last.
    #[test]
    fn a_counter_bumped_in_a_loop_survives_the_yields_in_it() {
        let src = "#[display_name = \"T\"]\n\
                   #[path = \"**/*.t\"]\n\
                   pub fn t() {\n\
                       if /\\[/ {\n\
                           var opens = 1;\n\
                           var closes = 0;\n\
                           loop {\n\
                               yield other;\n\
                               if /\\[/ { opens += 1; }\n\
                               else if /\\]/ {\n\
                                   closes += 1;\n\
                                   if opens == closes { yield string; break; }\n\
                               }\n\
                               else if /\\d/ { yield keyword; }\n\
                               else { break; }\n\
                           }\n\
                       }\n\
                       if /.*/ { yield other; }\n\
                   }\n";

        // The inner `]` must not close the outer array: only the last `]`
        // carries the string colour, and the tail is left alone.
        let spans = highlight(src, &["[[1]] tail"]);
        assert_eq!(
            spans[0],
            [
                ("other".to_string(), "[[".to_string()),
                ("keyword".to_string(), "1".to_string()),
                ("other".to_string(), "]".to_string()),
                ("string".to_string(), "]".to_string()),
                ("other".to_string(), " tail".to_string()),
            ]
        );
    }

    /// A loop skips characters none of its matchers care about before each
    /// iteration. Matchers reached through a call count too: without that,
    /// a loop whose only matchers live in a helper skipped straight past
    /// everything the helper would have coloured.
    #[test]
    fn a_loop_does_not_skip_what_its_callee_would_match() {
        let src = "#[display_name = \"T\"]\n\
                   #[path = \"**/*.t\"]\n\
                   pub fn t() {\n\
                       until /$/ {\n\
                           yield other;\n\
                           if /#/ { yield comment; }\n\
                           else { t_word(); }\n\
                       }\n\
                   }\n\
                   fn t_word() {\n\
                       if /\\w+/ { yield keyword; }\n\
                   }\n";

        // The skip runs between iterations, so the word has to sit behind a
        // character the loop itself does not match.
        let spans = highlight(src, &[" ab #"]);
        assert_eq!(
            spans[0],
            [
                ("other".to_string(), " ".to_string()),
                ("keyword".to_string(), "ab".to_string()),
                ("other".to_string(), " ".to_string()),
                ("comment".to_string(), "#".to_string()),
            ]
        );
    }

    /// An alternative that matched is given up when what follows it fails,
    /// and the next alternative is tried from the same offset. `in` matches
    /// the start of `invariant`, the `\>` fails, and `invariant` is the one
    /// that has to colour the word -- whatever order the list is in.
    #[test]
    fn an_alternative_is_retried_when_what_follows_it_fails() {
        let src = "#[display_name = \"T\"]\n\
                   #[path = \"**/*.t\"]\n\
                   pub fn t() {\n\
                       until /$/ {\n\
                           yield other;\n\
                           if /(?:in|invariant)\\>/ { yield keyword; }\n\
                           else if /\\w+/ {}\n\
                       }\n\
                   }\n";

        let spans = highlight(src, &["invariant in inv"]);
        assert_eq!(
            spans[0],
            [
                ("keyword".to_string(), "invariant".to_string()),
                ("other".to_string(), " ".to_string()),
                ("keyword".to_string(), "in".to_string()),
                ("other".to_string(), " inv".to_string()),
            ]
        );
    }

    /// The same retry through a capturing group: the capture is the
    /// alternative that made the whole pattern match, and a group after the
    /// alternation is numbered once however many alternatives it follows.
    #[test]
    fn a_retried_alternative_keeps_its_captures_straight() {
        let src = "#[display_name = \"T\"]\n\
                   #[path = \"**/*.t\"]\n\
                   pub fn t() {\n\
                       until /$/ {\n\
                           yield other;\n\
                           if /(BEGIN|BEGINFILE)\\>\\s*(\\w+)/ {\n\
                               yield $1 as keyword;\n\
                               yield other;\n\
                               yield $2 as string;\n\
                           }\n\
                           else if /\\w+/ {}\n\
                       }\n\
                   }\n";

        let spans = highlight(src, &["BEGINFILE x BEGIN y"]);
        assert_eq!(
            spans[0],
            [
                ("keyword".to_string(), "BEGINFILE".to_string()),
                ("other".to_string(), " ".to_string()),
                ("string".to_string(), "x".to_string()),
                ("other".to_string(), " ".to_string()),
                ("keyword".to_string(), "BEGIN".to_string()),
                ("other".to_string(), " ".to_string()),
                ("string".to_string(), "y".to_string()),
            ]
        );
    }

    /// The folded form of a keyword list, case-insensitive: the `\>` is part
    /// of the prefix check, so a prefix of a longer word is not a match.
    #[test]
    fn a_bounded_keyword_list_is_not_fooled_by_a_longer_word() {
        let src = "#[display_name = \"T\"]\n\
                   #[path = \"**/*.t\"]\n\
                   pub fn t() {\n\
                       until /$/ {\n\
                           yield other;\n\
                           if /(?i:(?:x|xy))\\>/ { yield keyword; }\n\
                           else if /\\w+/ {}\n\
                       }\n\
                   }\n";

        let spans = highlight(src, &["XY xz X"]);
        assert_eq!(
            spans[0],
            [
                ("keyword".to_string(), "XY".to_string()),
                ("other".to_string(), " xz ".to_string()),
                ("keyword".to_string(), "X".to_string()),
            ]
        );
    }

    /// `save $N` remembers a capture and `if $saved` tests for it on a later
    /// line -- the heredoc shape, where the delimiter is only known at the
    /// opener. The closing test consumes the span; anything else stays body.
    #[test]
    fn a_saved_capture_closes_a_block_on_a_later_line() {
        let src = "#[display_name = \"T\"]\n\
                   #[path = \"**/*.t\"]\n\
                   pub fn t() {\n\
                       if /<<(\\w+)/ {\n\
                           save $1;\n\
                           yield keyword;\n\
                           loop {\n\
                               await input;\n\
                               if $saved {\n\
                                   if /$/ { yield keyword; break; }\n\
                               }\n\
                               if /.*/ {}\n\
                               yield string;\n\
                           }\n\
                       }\n\
                       if /.*/ { yield other; }\n\
                   }\n";

        let spans = highlight(src, &["<<SQL", "select EOF", "SQLx", "SQL", "after"]);
        assert_eq!(spans[0], [("keyword".to_string(), "<<SQL".to_string())]);
        assert_eq!(spans[1], [("string".to_string(), "select EOF".to_string())]);
        // Starts with the delimiter but does not end there: still body.
        assert_eq!(spans[2], [("string".to_string(), "SQLx".to_string())]);
        assert_eq!(spans[3], [("keyword".to_string(), "SQL".to_string())]);
        assert_eq!(spans[4], [("other".to_string(), "after".to_string())]);
    }

    /// The remembered span survives a snapshot and restore, since the editor
    /// re-highlights from a cached line state and a heredoc body must still
    /// know its delimiter afterwards.
    #[test]
    fn a_saved_capture_survives_snapshot_and_restore() {
        let _ = stdext::arena::init(16 * 1024 * 1024);
        let arena = scratch_arena(None);
        let mut compiler = Compiler::new(&arena);
        compiler
            .parse(
                "test.lsh",
                "#[display_name = \"T\"]\n\
                 #[path = \"**/*.t\"]\n\
                 pub fn t() {\n\
                     if /<<(\\w+)/ { save $1; }\n\
                     if /.*/ {}\n\
                     loop {\n\
                         await input;\n\
                         if $saved { yield keyword; break; }\n\
                         if /.*/ { yield string; }\n\
                     }\n\
                 }\n",
            )
            .unwrap();
        let assembly = compiler.assemble().unwrap();
        let charsets: Vec<SerializedCharset> =
            assembly.charsets.iter().map(|cs| cs.serialize()).collect();
        let entry = assembly.entrypoints[0].address as u32;
        let mut runtime = Runtime::new(&assembly.instructions, &assembly.strings, &charsets, entry);

        runtime.parse_next_line::<u32>(&arena, b"<<END");
        let state = runtime.snapshot();

        // A fresh runtime restored from the snapshot must still close on END.
        let mut resumed = Runtime::new(&assembly.instructions, &assembly.strings, &charsets, entry);
        resumed.restore(&state);
        let body = resumed.parse_next_line::<u32>(&arena, b"body").spans;
        let close = resumed.parse_next_line::<u32>(&arena, b"END").spans;

        let kind_of = |hs: &BVec<'_, Highlight<u32>>| hs[0].kind;
        let keyword =
            assembly.highlight_kinds.iter().find(|hk| hk.identifier == "keyword").unwrap();
        let string = assembly.highlight_kinds.iter().find(|hk| hk.identifier == "string").unwrap();
        assert_eq!(kind_of(&body), string.value);
        assert_eq!(kind_of(&close), keyword.value);
    }

    /// `name = value;` writes the declared register rather than binding the
    /// name to a fresh one, so a flag raised inside a loop is what the test
    /// after the loop sees -- and a reset there is what the next iteration
    /// sees. Under the old rebinding both reads went to a register nothing
    /// wrote any more.
    #[test]
    fn a_flag_assigned_in_a_loop_is_visible_outside_it() {
        let src = "#[display_name = \"T\"]\n\
                   #[path = \"**/*.t\"]\n\
                   pub fn t() {\n\
                       var one = 1;\n\
                       var seen = 0;\n\
                       until /$/ {\n\
                           if /x/ { seen = 1; }\n\
                           if /.*/ {}\n\
                       }\n\
                       if seen == one { yield keyword; } else { yield string; }\n\
                   }\n";

        let spans = highlight(src, &["ab", "xb", "ab"]);
        assert_eq!(spans[0], [("string".to_string(), "ab".to_string())]);
        assert_eq!(spans[1], [("keyword".to_string(), "xb".to_string())]);
        assert_eq!(spans[2], [("string".to_string(), "ab".to_string())]);
    }

    /// Resuming from an `await input` lands on whatever follows it. When that
    /// is an already-serialized call, the generator has to jump to it: an
    /// inlined copy falls through into the code that happens to sit after it.
    #[test]
    fn a_call_after_an_await_resumes_the_call_and_not_its_neighbour() {
        let src = "#[display_name = \"T\"]\n\
                   #[path = \"**/*.t\"]\n\
                   fn t_word() {\n\
                       if /\\w+/ { yield keyword; }\n\
                   }\n\
                   pub fn t() {\n\
                       loop {\n\
                           await input;\n\
                           t_word();\n\
                           if /.*/ {}\n\
                           yield other;\n\
                       }\n\
                   }\n";

        // The first line runs the loop body straight through; the second one
        // reaches it by resuming from the await.
        let spans = highlight(src, &["one two", "three four"]);

        let expected = |word: &str, rest: &str| {
            [("keyword".to_string(), word.to_string()), ("other".to_string(), rest.to_string())]
        };
        assert_eq!(spans[0], expected("one", " two"));
        assert_eq!(spans[1], expected("three", " four"));
    }

    /// Consumers slice the line between consecutive starts, so a definition
    /// that yields capture groups out of order must not produce a span that
    /// starts before the previous one.
    #[test]
    fn spans_stay_monotonic_when_captures_are_yielded_out_of_order() {
        let src = "#[display_name = \"T\"]\n\
                   #[path = \"**/*.t\"]\n\
                   pub fn t() {\n\
                       if /(\\w+)\\s+(\\w+)/ { yield $2 as string; yield $1 as keyword; }\n\
                   }\n";
        let (spans, _) = captured(|| highlight(src, &["foo bar"]));
        assert_eq!(spans[0].iter().map(|(_, t)| t.as_str()).collect::<Vec<_>>(), ["foo ", "bar"]);
    }

    /// The justfile attribute bug: a loop guard that never matches and a
    /// body that never reaches the end of the line. Before the budget this
    /// test never returned.
    const STUCK_LOOP: &str = "#[display_name = \"T\"]\n\
                              #[path = \"**/*.t\"]\n\
                              pub fn t() {\n\
                                  if /\\[/ { until /\\]/ { yield keyword; } }\n\
                              }\n";

    #[test]
    fn a_stuck_loop_is_cut_off_and_the_next_line_starts_fresh() {
        let (spans, _) = captured(|| highlight(STUCK_LOOP, &["[unix", "[ok]"]));
        assert_eq!(spans[1].iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(), ["keyword"]);
    }

    #[cfg(feature = "sanity")]
    #[test]
    fn a_stuck_loop_trips_the_budget_check() {
        let (_, msgs) = captured(|| highlight(STUCK_LOOP, &["[unix"]));
        assert!(
            stdext::sanity::capture::fired(&msgs, "runtime_line_within_instruction_budget"),
            "{msgs:?}"
        );
    }

    /// Long but legal lines stay well inside the budget: a check that fires
    /// on awkward input is worse than none.
    #[cfg(feature = "sanity")]
    #[test]
    fn awkward_but_legal_lines_do_not_trip_the_budget_check() {
        let src = "#[display_name = \"T\"]\n\
                   #[path = \"**/*.t\"]\n\
                   pub fn t() {\n\
                       until /$/ {\n\
                           if /\"/ { until /$/ { if /\\\\./ {} else if /\"/ { yield string; break; } } }\n\
                           else if /\\w+/ { yield keyword; }\n\
                           else if /./ { yield other; }\n\
                       }\n\
                   }\n";
        let quotes = "\"".repeat(20_000);
        let words = "ab ".repeat(10_000);
        let escapes = "\"\\\"".repeat(5_000);
        let (_, msgs) = captured(|| highlight(src, &[&quotes, &words, &escapes]));
        assert!(msgs.is_empty(), "{msgs:?}");
    }

    #[test]
    fn an_empty_line_still_yields_two_spans() {
        let _ = stdext::arena::init(16 * 1024 * 1024);
        let arena = scratch_arena(None);
        let mut compiler = Compiler::new(&arena);
        compiler
            .parse(
                "test.lsh",
                "#[display_name = \"T\"]\n\
                 #[path = \"**/*.t\"]\n\
                 pub fn t() {\n\
                     if /.*/ { yield string; }\n\
                 }\n",
            )
            .unwrap();
        let assembly = compiler.assemble().unwrap();
        let charsets: Vec<SerializedCharset> =
            assembly.charsets.iter().map(|cs| cs.serialize()).collect();
        let entry = assembly.entrypoints[0].address as u32;
        let mut runtime = Runtime::new(&assembly.instructions, &assembly.strings, &charsets, entry);

        let spans = runtime.parse_next_line::<u32>(&arena, b"").spans;
        assert_eq!(spans.len(), 2);
        assert_eq!((spans[0].start, spans[1].start), (0, 0));
    }
}
