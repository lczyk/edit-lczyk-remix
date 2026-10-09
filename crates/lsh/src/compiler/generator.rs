//! Code generators for different output formats.
//!
//! ## TODO
//!
//! - The label lookup (with a `HashMap`) is $NOT_GREAT.
//!   The backend should emit metadata, I think.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::read_dir;
use std::io;
use std::path::PathBuf;

use stdext::arena::scratch_arena;

use super::*;
use crate::kind;
use crate::runtime::{Instruction, MnemonicFormattingConfig};

pub struct Generator<'a> {
    compiler: Compiler<'a>,
}

impl<'a> Generator<'a> {
    pub fn new(arena: &'a Arena) -> Self {
        Self { compiler: Compiler::new(arena) }
    }

    pub fn read_file(&mut self, path: &Path) -> CompileResult<()> {
        let path_str = path.display().to_string();
        match std::fs::read_to_string(path) {
            Ok(src) => self.compiler.parse(&path_str, &src),
            Err(e) => {
                Err(CompileError { path: path_str, line: 0, column: 0, message: e.to_string() })
            }
        }
    }

    pub fn read_directory(&mut self, path: &Path) -> CompileResult<()> {
        let files = Self::read_dir_to_vec(path).map_err(|e| CompileError {
            path: path.display().to_string(),
            line: 0,
            column: 0,
            message: e.to_string(),
        })?;

        for path in files {
            if path.extension() == Some(OsStr::new("lsh")) {
                self.read_file(&path)?;
            }
        }
        Ok(())
    }

    fn read_dir_to_vec(path: &Path) -> io::Result<Vec<PathBuf>> {
        let mut paths = Vec::new();

        for entry in read_dir(path)? {
            let entry = entry?;
            if entry.metadata().is_ok_and(|f| f.is_file())
                && entry.file_name().as_encoded_bytes().ends_with(b".lsh")
            {
                paths.push(entry.path());
            }
        }

        paths.sort_unstable();
        Ok(paths)
    }

    pub fn assemble(mut self) -> CompileResult<Assembly<'a>> {
        self.compiler.assemble()
    }

    pub fn generate_assembly(mut self, vt: bool) -> CompileResult<String> {
        let mut output = String::new();
        let assembly = self.compiler.assemble()?;
        let line_num_width = assembly.instructions.len().checked_ilog10().unwrap_or(0) as usize + 1;
        let mnemonic_config = if vt {
            MnemonicFormattingConfig {
                instruction_prefix: "\x1b[33m", // yellow
                instruction_suffix: "\x1b[39m", // default

                register_prefix: "\x1b[32m", // green
                register_suffix: "\x1b[39m",

                address_prefix: "\x1b[90m", // bright black
                address_suffix: "\x1b[39m",

                numeric_prefix: "\x1b[36m", // cyan
                numeric_suffix: "\x1b[39m",
            }
        } else {
            Default::default()
        };
        let label_prefix = if vt { "\x1b[4;94m" } else { "" }; // underlined & bright blue
        let label_suffix = if vt { "\x1b[m" } else { "" };
        let line_prefix = if vt { "\x1b[90m" } else { "" }; // bright black
        let comment_prefix = if vt { "\x1b[32m" } else { "" }; // green
        let comment_suffix = if vt { "\x1b[39m" } else { "" };

        // TODO: This is kind of stupid. There should be per-instruction annotations.
        let labels: HashMap<usize, &str> =
            assembly.entrypoints.iter().map(|ep| (ep.address, ep.name.as_str())).collect();

        let mut off = 0;
        while off < assembly.instructions.len() {
            if let Some(label) = labels.get(&off) {
                if off != 0 {
                    output.push('\n');
                }
                _ = writeln!(output, "{label_prefix}{}:{label_suffix}", label);
            }

            let (Some(instr), len) = Instruction::decode(&assembly.instructions[off..]) else {
                break;
            };

            let scratch = scratch_arena(None);
            let mnemonic = instr.mnemonic(&scratch, &mnemonic_config);
            _ = write!(output, "{line_prefix}{off:>line_num_width$}:  {mnemonic}");

            let text_chars = {
                let mut count = 0;
                let mut in_escape = false;
                for c in mnemonic.bytes() {
                    if in_escape {
                        if c.is_ascii_alphabetic() {
                            in_escape = false;
                        }
                    } else if c == b'\x1b' {
                        in_escape = true;
                    } else {
                        count += 1;
                    }
                }
                count
            };
            let padding_width = 40usize.saturating_sub(text_chars);
            _ = write!(output, "{:<padding_width$}", "");

            match instr {
                Instruction::JumpIfMatchCharset { idx, .. } => {
                    _ = write!(
                        output,
                        " {comment_prefix}// {:?}{comment_suffix}",
                        assembly.charsets[idx as usize]
                    )
                }
                Instruction::JumpIfMatchPrefix { idx, .. }
                | Instruction::JumpIfMatchPrefixInsensitive { idx, .. }
                | Instruction::JumpIfMatchPrefixBounded { idx, .. }
                | Instruction::JumpIfMatchPrefixInsensitiveBounded { idx, .. } => {
                    _ = write!(
                        output,
                        " {comment_prefix}// {:?}{comment_suffix}",
                        assembly.strings[idx as usize]
                    )
                }
                _ => {}
            }

            output.push('\n');
            off += len;
        }

        Ok(output)
    }

    pub fn generate_rust(mut self) -> CompileResult<String> {
        let assembly = self.compiler.assemble()?;

        let mut output = String::new();
        output.push_str("// This file is auto-generated. Do not edit it manually.\n\n");
        output.push_str("use lsh::runtime::Language;\n\n");

        output.push_str(
            "#[repr(u8)]\n#[derive(Debug, Clone, Copy, PartialEq, Eq)]\npub enum HighlightKind {\n",
        );
        let members: Vec<_> = assembly
            .highlight_kinds
            .iter()
            .map(|hk| (hk, format!("{} = {},", hk.fmt_camelcase(), hk.value)))
            .collect();
        let width = members.iter().map(|s| s.1.len()).max().unwrap_or(0);
        for (hk, member) in members {
            _ = writeln!(output, "    {member:<width$} // {}", hk.identifier);
        }
        output.push_str("}\n");
        // TryFrom below transmutes a u8; the repr makes that a guarantee
        // rather than a layout accident, and the count keeps it in range.
        output.push_str("\nconst _: () = assert!(HighlightKind::COUNT <= 256);\n");
        // The runtime emits builtin kinds by value; hold the enum to the
        // pinned order so a stale table can't compile.
        for hk in assembly
            .highlight_kinds
            .iter()
            .filter(|hk| kind::builtin_value(hk.identifier).is_some())
        {
            _ = writeln!(
                output,
                "const _: () = assert!(HighlightKind::{} as u32 == {});",
                hk.fmt_camelcase(),
                hk.value
            );
        }

        if let Some(last) = assembly.highlight_kinds.last() {
            _ = write!(
                output,
                "
impl TryFrom<u32> for HighlightKind {{
    type Error = ();

    #[inline]
    fn try_from(value: u32) -> Result<Self, Self::Error> {{
        if value <= Self::{} as u32 {{
            Ok(unsafe {{ std::mem::transmute::<u8, Self>(value as u8) }})
        }} else {{
            Err(())
        }}
    }}
}}
",
                last.fmt_camelcase()
            );
        }

        // Canonical default colour per highlight kind. Single source of
        // truth so all consumers (eat, edit, future tools) agree on the
        // out-of-the-box palette regardless of which language definitions
        // are loaded. Kinds without a colour fall through to `None`.
        output.push_str("\nimpl HighlightKind {\n");
        _ = writeln!(
            output,
            "    /// Number of highlight kinds; discriminants are contiguous `0..COUNT`.\n    pub const COUNT: usize = {};\n",
            assembly.highlight_kinds.len()
        );
        output.push_str("    /// Canonical default colour for this highlight kind.\n");
        output.push_str("    /// `None` for kinds that have no colour by default\n");
        output.push_str("    /// (e.g. `markup.bold`, `markup.italic` -- those need attribute\n");
        output.push_str("    /// rendering, not a foreground colour).\n");
        output.push_str("    pub fn default_color(self) -> Option<lsh::runtime::Ansi16> {\n");
        output.push_str("        use lsh::runtime::Ansi16;\n");
        output.push_str("        match self {\n");
        for hk in &assembly.highlight_kinds {
            if let Some(colour) = default_ansi16(hk.identifier) {
                _ = writeln!(
                    output,
                    "            HighlightKind::{} => Some(Ansi16::{colour:?}),",
                    hk.fmt_camelcase()
                );
            }
        }
        output.push_str("            _ => None,\n");
        output.push_str("        }\n");
        output.push_str("    }\n");
        output.push_str("}\n");

        // The mermaid dump is wrapped in a rust block comment. Rust block
        // comments nest, so any `/*` or `*/` in the diagram (regex literals
        // like the block-comment delimiters leak in as node labels) would
        // unbalance the nesting and leave the comment unterminated.
        output.push_str("/*\n");
        output.push_str(&escape_for_block_comment(&self.compiler.as_mermaid()));
        output.push_str("*/\n");

        output.push_str("\n#[rustfmt::skip]\n");
        output.push_str("pub static EMPTY_SHEBANGS: &[&str] = &[];\n");
        for (idx, ep) in assembly.entrypoints.iter().enumerate() {
            if !ep.shebangs.is_empty() {
                _ = write!(output, "#[rustfmt::skip]\npub static SHEBANGS_{idx}: &[&str] = &[");
                for (i, s) in ep.shebangs.iter().enumerate() {
                    if i > 0 {
                        output.push_str(", ");
                    }
                    _ = write!(output, "{s:?}");
                }
                output.push_str("];\n");
            }
        }
        output.push_str("\n#[rustfmt::skip] pub const LANGUAGES: &[Language] = &[\n");
        for (idx, ep) in assembly.entrypoints.iter().enumerate() {
            let shebangs = if ep.shebangs.is_empty() {
                "EMPTY_SHEBANGS".to_string()
            } else {
                format!("SHEBANGS_{idx}")
            };
            _ = writeln!(
                output,
                "    Language {{ id: {:?}, name: {:?}, line_comment: {}, block_comment: {}, shebangs: {shebangs}, entrypoint: {}, detect_entrypoint: {} }},",
                ep.name.replace('_', "-"),
                ep.display_name,
                match &ep.line_comment {
                    Some(s) => format!("Some({s:?})"),
                    None => "None".to_string(),
                },
                match &ep.block_comment {
                    Some((o, c)) => format!("Some(({o:?}, {c:?}))"),
                    None => "None".to_string(),
                },
                ep.address,
                match ep.detect_address {
                    Some(addr) => format!("Some({addr})"),
                    None => "None".to_string(),
                },
            );
        }
        output.push_str("];\n");

        // The language for files nothing else claims; consumers end their
        // detection chain here rather than in an Option.
        if let Some(idx) = assembly.entrypoints.iter().position(|ep| ep.name == "plain") {
            _ = writeln!(output, "\npub static PLAIN: &Language = &LANGUAGES[{idx}];");
        }

        output.push_str(
            "\n#[rustfmt::skip] pub const FILE_ASSOCIATIONS: &[(&str, &Language)] = &[\n",
        );
        for (idx, ep) in assembly.entrypoints.iter().enumerate() {
            for path in &ep.paths {
                _ = writeln!(output, "    ({path:?}, &LANGUAGES[{idx}]),");
            }
        }
        output.push_str("];\n");

        _ = writeln!(
            output,
            "\n#[rustfmt::skip] pub static ASSEMBLY: [u8; {len}] = [",
            len = assembly.instructions.len() + Instruction::MAX_ENCODED_SIZE,
        );
        let line_num_width = assembly.instructions.len().checked_ilog10().unwrap_or(0) as usize + 1;

        // TODO: This is kind of stupid. There should be per-instruction annotations.
        let labels: HashMap<usize, &str> =
            assembly.entrypoints.iter().map(|ep| (ep.address, ep.name.as_str())).collect();

        let mut off = 0;
        while off < assembly.instructions.len() {
            if let Some(label) = labels.get(&off) {
                if off != 0 {
                    output.push('\n');
                }
                _ = writeln!(output, "    // {}:", label);
            }

            output.push_str("    ");

            let (instr, len) = Instruction::decode(&assembly.instructions[off..]);
            let scratch = scratch_arena(None);
            for i in 0..len {
                _ = write!(output, "0x{:02x}, ", assembly.instructions[off + i]);
            }

            if let Some(instr) = instr {
                _ = writeln!(
                    output,
                    "{:<padding_width$}// {off:>line_num_width$}:  {mnemonic}",
                    "",
                    padding_width = Instruction::MAX_ENCODED_SIZE.saturating_sub(len) * 6,
                    mnemonic = instr.mnemonic(&scratch, &Default::default())
                );
            } else {
                output.push('\n');
            }

            off += len;
        }
        // Normally the runtime would need to do bounds checks at all times to be safe,
        // since there may be malformed bytecode (e.g. a bug in this compiler).
        // We can fix that by padding the instruction stream with invalid opcodes at the end.
        // This works as long as the runtime checks for valid opcodes. Even if the last valid
        // opcode is chopped off (due to a bug above), the runtime can do an unchecked read
        // of `MAX_ENCODED_SIZE` bytes without risking OOB access.
        output.push_str("\n    // padding\n");
        for _ in 0..Instruction::MAX_ENCODED_SIZE {
            _ = writeln!(output, "    0xff,");
        }
        output.push_str("];\n");

        _ = writeln!(
            output,
            "\n#[rustfmt::skip] pub static CHARSETS: [[u16; 16]; {len}] = [",
            len = assembly.charsets.len(),
        );
        for cs in assembly.charsets {
            let cs = cs.serialize();
            output.push_str("    [");
            for (i, &v) in cs.iter().enumerate() {
                if i != 0 {
                    _ = write!(output, ", ");
                }
                _ = write!(output, "0x{v:04x}");
            }
            output.push_str("],\n");
        }
        output.push_str("];\n");

        _ = writeln!(
            output,
            "\n#[rustfmt::skip] pub static STRINGS: [&str; {len}] = [",
            len = assembly.strings.len(),
        );
        for s in assembly.strings {
            _ = writeln!(output, "    {s:?},");
        }
        output.push_str("];\n");

        Ok(output)
    }
}

/// Canonical default ANSI-16 colour for a highlight-kind identifier
/// (the `dotted.lower.snake` form as it appears in `.lsh` `yield` statements).
/// `None` means "no default colour" -- the consumer may still apply text
/// attributes (bold, italic, etc.).
///
/// Public so consumers outside the generator (lsh-bin, and anything else
/// rendering highlights) read the same table rather than transcribing it.
pub fn default_ansi16(identifier: &str) -> Option<crate::runtime::Ansi16> {
    use crate::runtime::Ansi16;
    Some(match identifier {
        "comment" => Ansi16::Green,
        "method" => Ansi16::BrightYellow,
        "string" => Ansi16::BrightRed,
        "variable" => Ansi16::BrightCyan,
        "constant.character.escape" => Ansi16::Yellow,
        "constant.language" => Ansi16::BrightBlue,
        "constant.numeric" => Ansi16::BrightGreen,
        "keyword.control" => Ansi16::BrightMagenta,
        "keyword.other" => Ansi16::BrightBlue,
        "storage.type" => Ansi16::Cyan,
        "support.function" => Ansi16::Yellow,
        "markup.changed" => Ansi16::BrightBlue,
        "markup.conflict.marker" => Ansi16::Magenta,
        "markup.deleted" => Ansi16::BrightRed,
        "markup.heading" => Ansi16::BrightBlue,
        "markup.inserted" => Ansi16::BrightGreen,
        "markup.list" => Ansi16::BrightBlue,
        "meta.header" => Ansi16::BrightBlue,
        // Rainbow-csv column cycle. Ordered for adjacent-column contrast;
        // reuse of hues already taken by semantic kinds is fine -- the two
        // never appear in the same file.
        "rainbow.1" => Ansi16::BrightYellow,
        "rainbow.2" => Ansi16::BrightCyan,
        "rainbow.3" => Ansi16::BrightMagenta,
        "rainbow.4" => Ansi16::BrightGreen,
        "rainbow.5" => Ansi16::BrightBlue,
        "rainbow.6" => Ansi16::BrightRed,
        _ => return None,
    })
}

/// Break up `/*` and `*/` so the text can be safely embedded inside a rust
/// block comment. Rust block comments nest, so an unbalanced delimiter in the
/// embedded text leaves the comment unterminated and breaks the whole file.
/// Used for the mermaid diagram dump, where regex literals leak in as labels.
fn escape_for_block_comment(s: &str) -> String {
    s.replace("/*", "/ *").replace("*/", "* /")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A definition whose regexes carry an odd number of `/*` / `*/` (here two
    /// opens, one close) used to leak into the mermaid block comment and leave
    /// it unterminated -- 68 downstream compile errors. The escape keeps the
    /// embedded text free of either delimiter.
    #[test]
    fn mermaid_block_comment_stays_balanced() {
        let arena = Arena::new(1 << 20).unwrap();
        let mut generator = Generator::new(&arena);
        generator
            .compiler
            .parse(
                "test.lsh",
                "#[display_name = \"T\"]\n\
                 #[path = \"**/*.t\"]\n\
                 pub fn t() {\n\
                 if /\\/\\*/ {}\n\
                 until /$/ { if /\\/\\*/ { if /\\*\\// {} } }\n\
                 }\n",
            )
            .unwrap();
        let rust = generator.generate_rust().unwrap();

        // Slice out the mermaid block comment (the first `/* ... */` block) and
        // assert it carries neither delimiter, so its nesting is balanced.
        let start = rust.find("/*\n").expect("mermaid comment open");
        let body = &rust[start + 3..];
        let end = body.find("*/\n").expect("mermaid comment close");
        let mermaid = &body[..end];
        assert!(!mermaid.contains("/*"), "mermaid leaks a `/*`");
        assert!(!mermaid.contains("*/"), "mermaid leaks a `*/`");
    }

    #[test]
    fn the_kind_enum_pins_its_layout() {
        let arena = Arena::new(1 << 20).unwrap();
        let mut generator = Generator::new(&arena);
        generator
            .compiler
            .parse(
                "test.lsh",
                "#[display_name = \"T\"]\n\
                 #[path = \"**/*.t\"]\n\
                 pub fn t() { if /x/ { yield keyword; } }\n",
            )
            .unwrap();
        let rust = generator.generate_rust().unwrap();
        assert!(rust.contains(
            "#[repr(u8)]\n#[derive(Debug, Clone, Copy, PartialEq, Eq)]\npub enum HighlightKind"
        ));
        assert!(rust.contains("const _: () = assert!(HighlightKind::COUNT <= 256);"));
        // Builtin kinds come first, in kind::BUILTIN order, ahead of any
        // yielded kind however it sorts.
        assert!(rust.contains("    Other = 0,"), "{rust}");
        assert!(rust.contains("    MarkupConflictMarker = 1,"), "{rust}");
        assert!(rust.contains("    Keyword = 2,"), "{rust}");
        assert!(
            rust.contains(
                "const _: () = assert!(HighlightKind::MarkupConflictMarker as u32 == 1);"
            )
        );
    }

    #[test]
    fn escape_breaks_both_delimiters() {
        assert_eq!(escape_for_block_comment("a /* b */ c"), "a / * b * / c");
        assert_eq!(escape_for_block_comment("no delims"), "no delims");
    }
}
