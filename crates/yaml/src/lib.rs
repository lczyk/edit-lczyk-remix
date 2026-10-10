//! A small YAML reader for config files.
//!
//! Covers JSON plus the block syntax a hand-written config reaches for: block
//! mappings and sequences (including a sequence at its key's indentation),
//! flow `[...]` / `{...}` spanning lines, plain, single- and double-quoted
//! scalars, `|` / `>` block scalars with chomping and indentation indicators,
//! comments, and the `---` / `...` markers of a single document.
//!
//! Plain scalars resolve per the YAML 1.2 core schema, so `no`, `on` and `y`
//! stay strings. Mapping keys are kept as their source text.
//!
//! Anchors, aliases, tags, directives, complex keys, multi-line plain scalars
//! and multiple documents are rejected with an error rather than misread.

use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    Seq(Vec<Value>),
    /// Entries in document order. Keys are unique.
    Map(Vec<(String, Value)>),
}

impl Value {
    pub fn as_str(&self) -> Option<&str> {
        if let Value::String(s) = self { Some(s) } else { None }
    }

    pub fn as_seq(&self) -> Option<&[Value]> {
        if let Value::Seq(v) = self { Some(v) } else { None }
    }

    pub fn as_map(&self) -> Option<&[(String, Value)]> {
        if let Value::Map(m) = self { Some(m) } else { None }
    }

    /// The value under `key`, when `self` is a mapping that has it.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.as_map()?.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }
}

/// A parse failure. `line` and `col` are 1-based; `col` counts characters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub line: usize,
    pub col: usize,
    pub msg: &'static str,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}: {}", self.line, self.col, self.msg)
    }
}

impl std::error::Error for Error {}

type Result<T> = std::result::Result<T, Error>;

/// Parse a single YAML document. An empty document is [`Value::Null`].
pub fn parse(text: &str) -> Result<Value> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    Parser { src: text, b: text.as_bytes(), pos: 0 }.document()
}

enum Scalar<'a> {
    Plain(&'a str),
    Quoted(String),
}

impl Scalar<'_> {
    fn into_key(self) -> String {
        match self {
            Scalar::Plain(s) => s.to_string(),
            Scalar::Quoted(s) => s,
        }
    }

    fn into_value(self) -> Value {
        match self {
            Scalar::Plain(s) => resolve_plain(s),
            Scalar::Quoted(s) => Value::String(s),
        }
    }
}

#[derive(Clone, Copy)]
enum Chomp {
    Clip,
    Strip,
    Keep,
}

fn is_blank(c: u8) -> bool {
    c == b' ' || c == b'\t'
}

fn is_break(c: u8) -> bool {
    c == b'\n' || c == b'\r'
}

fn is_flow_indicator(c: u8) -> bool {
    matches!(c, b',' | b'[' | b']' | b'{' | b'}')
}

struct Parser<'a> {
    src: &'a str,
    b: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn eof(&self) -> bool {
        self.pos >= self.b.len()
    }

    fn peek(&self) -> u8 {
        self.peek_at(0)
    }

    fn peek_at(&self, off: usize) -> u8 {
        self.b.get(self.pos + off).copied().unwrap_or(0)
    }

    /// Whether the byte at `off` ends a token: a blank, a line break or EOF.
    fn ends_token_at(&self, off: usize) -> bool {
        let i = self.pos + off;
        i >= self.b.len() || is_blank(self.b[i]) || is_break(self.b[i])
    }

    fn line_start(&self) -> usize {
        self.b[..self.pos].iter().rposition(|&c| c == b'\n').map_or(0, |i| i + 1)
    }

    fn col(&self) -> usize {
        self.pos - self.line_start()
    }

    fn err(&self, msg: &'static str) -> Error {
        let pos = self.pos.min(self.b.len());
        let line = self.b[..pos].iter().filter(|&&c| c == b'\n').count() + 1;
        let start = self.b[..pos].iter().rposition(|&c| c == b'\n').map_or(0, |i| i + 1);
        let col = self.src[start..pos].chars().count() + 1;
        Error { line, col, msg }
    }

    fn skip_blanks(&mut self) {
        while is_blank(self.peek()) {
            self.pos += 1;
        }
    }

    fn skip_to_break(&mut self) {
        while !self.eof() && !is_break(self.peek()) {
            self.pos += 1;
        }
    }

    /// Consume one line break, if there is one.
    fn eat_break(&mut self) -> bool {
        match self.peek() {
            b'\r' if self.peek_at(1) == b'\n' => self.pos += 2,
            b'\n' | b'\r' => self.pos += 1,
            _ => return false,
        }
        true
    }

    /// A `#` starts a comment only after whitespace; elsewhere it is text.
    fn comment_allowed(&self) -> bool {
        self.pos == 0 || is_blank(self.b[self.pos - 1]) || is_break(self.b[self.pos - 1])
    }

    /// Whether the cursor, past any blanks, sits on a comment or a line end.
    fn at_line_end(&mut self) -> bool {
        self.skip_blanks();
        self.eof() || is_break(self.peek()) || self.peek() == b'#'
    }

    /// Consume the rest of the line, which may only hold blanks and a comment.
    fn end_of_line(&mut self) -> Result<()> {
        self.skip_blanks();
        if self.peek() == b'#' {
            if !self.comment_allowed() {
                return Err(self.err("a comment needs whitespace before the '#'"));
            }
            self.skip_to_break();
        }
        if self.eof() || self.eat_break() {
            Ok(())
        } else {
            Err(self.err("unexpected text after the value"))
        }
    }

    fn at_marker(&self, marker: &[u8; 3]) -> bool {
        self.b[self.pos..].starts_with(marker) && self.ends_token_at(3)
    }

    /// Move to the start of the next line with content, past blank and
    /// comment-only lines, and return its indentation without consuming it.
    /// `None` at EOF or at a document marker.
    fn peek_line(&mut self) -> Result<Option<usize>> {
        while !self.eof() {
            let start = self.pos;
            let mut i = 0;
            while self.peek_at(i) == b' ' {
                i += 1;
            }
            let mut j = i;
            while is_blank(self.peek_at(j)) {
                j += 1;
            }
            let c = self.peek_at(j);
            if self.pos + j >= self.b.len() || is_break(c) || c == b'#' {
                self.pos += j;
                self.skip_to_break();
                self.eat_break();
                continue;
            }
            if j != i {
                self.pos = start + i;
                return Err(self.err("tabs are not allowed in indentation"));
            }
            if i == 0 && (self.at_marker(b"---") || self.at_marker(b"...")) {
                return Ok(None);
            }
            return Ok(Some(i));
        }
        Ok(None)
    }

    fn document(&mut self) -> Result<Value> {
        let mut value = Value::Null;
        if let Some(indent) = self.peek_line()? {
            self.pos += indent;
            value = self.block_node(-1, true)?;
        } else if self.at_marker(b"---") {
            self.pos += 3;
            if self.at_line_end() {
                self.end_of_line()?;
                if let Some(indent) = self.peek_line()? {
                    self.pos += indent;
                    value = self.block_node(-1, true)?;
                }
            } else {
                value = self.block_node(-1, false)?;
            }
        }

        if self.peek_line()?.is_some() {
            return Err(self.err("unexpected content after the document"));
        }
        if self.at_marker(b"...") {
            self.pos += 3;
            self.end_of_line()?;
            if self.peek_line()?.is_some() || self.at_marker(b"...") {
                return Err(self.err("unexpected content after the document"));
            }
        }
        if self.at_marker(b"---") {
            return Err(self.err("multiple documents are not supported"));
        }
        Ok(value)
    }

    /// Reject what may not start a plain scalar, with a message that says why.
    fn check_plain_start(&self, flow: bool) -> Result<()> {
        let msg = match self.peek() {
            b'&' => "anchors are not supported",
            b'*' => "aliases are not supported; quote a value that starts with '*'",
            b'!' => "tags are not supported",
            b'%' | b'@' | b'`' => "reserved character; quote a value that starts with it",
            b'?' if self.ends_token_at(1) => "complex keys are not supported",
            b':' if self.ends_token_at(1) => "expected a key before ':'",
            b'-' if !flow && self.ends_token_at(1) => "unexpected block sequence entry",
            b',' | b']' | b'}' => "unexpected flow indicator",
            _ => return Ok(()),
        };
        Err(self.err(msg))
    }

    /// Scan a plain scalar on the current line. Stops before `: `, ` #`, the
    /// line end and, in flow context, the flow indicators.
    fn plain(&mut self, flow: bool) -> &'a str {
        let start = self.pos;
        let mut end = start;
        while !self.eof() {
            let c = self.peek();
            if is_break(c)
                || (c == b':'
                    && (self.ends_token_at(1) || (flow && is_flow_indicator(self.peek_at(1)))))
                || (c == b'#' && self.pos > start && is_blank(self.b[self.pos - 1]))
                || (flow && is_flow_indicator(c))
            {
                break;
            }
            self.pos += 1;
            if !is_blank(c) {
                end = self.pos;
            }
        }
        // Blanks before a stop are trimmed but still consumed.
        &self.src[start..end]
    }

    fn scalar(&mut self, flow: bool) -> Result<Scalar<'a>> {
        match self.peek() {
            b'"' => self.double_quoted().map(Scalar::Quoted),
            b'\'' => self.single_quoted().map(Scalar::Quoted),
            _ => {
                self.check_plain_start(flow)?;
                Ok(Scalar::Plain(self.plain(flow)))
            }
        }
    }

    /// A node starting at the cursor. `parent` is the indentation of the
    /// enclosing block collection, -1 at the root. `allow_collection` is false
    /// for a value on its key's line, where only a scalar may start.
    fn block_node(&mut self, parent: isize, allow_collection: bool) -> Result<Value> {
        let col = self.col();
        match self.peek() {
            b'-' if self.ends_token_at(1) => {
                if !allow_collection {
                    return Err(self.err("a block sequence must start on its own line"));
                }
                self.block_seq(col)
            }
            b'|' | b'>' => self.block_scalar(parent),
            b'[' | b'{' => {
                let v = self.flow_node()?;
                self.end_of_line()?;
                Ok(v)
            }
            _ => {
                let start = self.pos;
                let scalar = self.scalar(false)?;
                self.skip_blanks();
                if self.peek() == b':' && self.ends_token_at(1) {
                    if !allow_collection {
                        return Err(self.err("a nested mapping must start on its own line"));
                    }
                    self.pos = start;
                    return self.block_map(col);
                }
                self.end_of_line()?;
                Ok(scalar.into_value())
            }
        }
    }

    /// The value after a `key:` or a `- `, on the same line or indented below.
    fn block_value(&mut self, parent: isize, after_key: bool) -> Result<Value> {
        if !self.at_line_end() {
            return self.block_node(parent, !after_key);
        }
        self.end_of_line()?;
        match self.peek_line()? {
            Some(indent) if indent as isize > parent => {
                self.pos += indent;
                self.block_node(parent, true)
            }
            // A sequence may sit at its key's own indentation.
            Some(indent)
                if after_key
                    && indent as isize == parent
                    && self.peek_at(indent) == b'-'
                    && self.ends_token_at(indent + 1) =>
            {
                self.pos += indent;
                self.block_seq(indent)
            }
            _ => Ok(Value::Null),
        }
    }

    fn block_map(&mut self, indent: usize) -> Result<Value> {
        let mut map: Vec<(String, Value)> = Vec::new();
        loop {
            let key_pos = self.pos;
            if matches!(self.peek(), b'[' | b'{') {
                return Err(self.err("flow collections as keys are not supported"));
            }
            let key = self.scalar(false)?;
            if matches!(key, Scalar::Plain("")) {
                return Err(self.err("expected a mapping key"));
            }
            let key = key.into_key();
            self.skip_blanks();
            if !(self.peek() == b':' && self.ends_token_at(1)) {
                return Err(self.err("expected ':' after the key"));
            }
            if map.iter().any(|(k, _)| *k == key) {
                self.pos = key_pos;
                return Err(self.err("duplicate key"));
            }
            self.pos += 1;
            let value = self.block_value(indent as isize, true)?;
            map.push((key, value));

            match self.peek_line()? {
                Some(i) if i == indent => self.pos += i,
                Some(i) if i > indent => {
                    self.pos += i;
                    return Err(self.err("unexpected indentation"));
                }
                _ => return Ok(Value::Map(map)),
            }
        }
    }

    fn block_seq(&mut self, indent: usize) -> Result<Value> {
        let mut seq = Vec::new();
        loop {
            self.pos += 1; // the '-'
            seq.push(self.block_value(indent as isize, false)?);

            match self.peek_line()? {
                Some(i) if i == indent && self.peek_at(i) == b'-' && self.ends_token_at(i + 1) => {
                    self.pos += i;
                }
                Some(i) if i > indent => {
                    self.pos += i;
                    return Err(self.err("unexpected indentation"));
                }
                _ => return Ok(Value::Seq(seq)),
            }
        }
    }

    fn block_scalar(&mut self, parent: isize) -> Result<Value> {
        let folded = self.peek() == b'>';
        self.pos += 1;
        let mut chomp = Chomp::Clip;
        let mut explicit = None;
        for _ in 0..2 {
            match self.peek() {
                b'-' => chomp = Chomp::Strip,
                b'+' => chomp = Chomp::Keep,
                c @ b'1'..=b'9' => explicit = Some((c - b'0') as isize),
                _ => break,
            }
            self.pos += 1;
        }
        self.end_of_line()?;

        let indent = match explicit {
            Some(m) => (parent.max(0) + m) as usize,
            None => {
                let detected = self.detect_indent();
                match detected {
                    Some(d) if d as isize > parent => d,
                    _ => (parent + 1) as usize,
                }
            }
        };

        let mut lines: Vec<&str> = Vec::new();
        while !self.eof() {
            let start = self.pos;
            self.skip_to_break();
            let line = &self.src[start..self.pos];
            let spaces = line.bytes().take_while(|&c| c == b' ').count();
            if spaces == line.len() {
                lines.push(line.get(indent..).unwrap_or(""));
            } else if spaces < indent {
                self.pos = start;
                break;
            } else {
                lines.push(&line[indent..]);
            }
            self.eat_break();
        }

        let content = lines.iter().rposition(|l| !l.is_empty()).map_or(0, |i| i + 1);
        let trailing = lines.len() - content;
        let body = &lines[..content];
        let mut out = if folded { fold(body) } else { body.join("\n") };
        if !body.is_empty() && !matches!(chomp, Chomp::Strip) {
            out.push('\n');
        }
        if matches!(chomp, Chomp::Keep) {
            out.extend(std::iter::repeat_n('\n', trailing));
        }
        Ok(Value::String(out))
    }

    /// Indentation of the first non-blank line ahead, without consuming it.
    fn detect_indent(&self) -> Option<usize> {
        let mut i = self.pos;
        while i < self.b.len() {
            let spaces = self.b[i..].iter().take_while(|&&c| c == b' ').count();
            let j = i + spaces;
            if j < self.b.len() && !is_break(self.b[j]) {
                return Some(spaces);
            }
            i = j + 1;
            if self.b.get(j) == Some(&b'\r') && self.b.get(i) == Some(&b'\n') {
                i += 1;
            }
        }
        None
    }

    /// At a line break inside a quoted scalar: drop the trailing blanks in
    /// `out` past `keep`, then fold -- one break is a space, each further
    /// (empty) line a newline.
    fn fold_quoted_break(&mut self, out: &mut String, keep: usize) {
        out.truncate(keep);
        let mut breaks = 0;
        while self.eat_break() {
            breaks += 1;
            self.skip_blanks();
        }
        if breaks == 1 {
            out.push(' ');
        } else {
            out.extend(std::iter::repeat_n('\n', breaks - 1));
        }
    }

    fn next_char(&mut self) -> char {
        let c = self.src[self.pos..].chars().next().unwrap_or('\0');
        self.pos += c.len_utf8();
        c
    }

    fn single_quoted(&mut self) -> Result<String> {
        let open = self.pos;
        self.pos += 1;
        let mut out = String::new();
        let mut keep = 0;
        loop {
            match self.peek() {
                _ if self.eof() => {
                    self.pos = open;
                    return Err(self.err("unterminated string"));
                }
                b'\'' if self.peek_at(1) == b'\'' => {
                    self.pos += 2;
                    out.push('\'');
                }
                b'\'' => {
                    self.pos += 1;
                    return Ok(out);
                }
                b'\n' | b'\r' => self.fold_quoted_break(&mut out, keep),
                c => {
                    let ch = self.next_char();
                    out.push(ch);
                    if is_blank(c) {
                        continue;
                    }
                }
            }
            keep = out.len();
        }
    }

    fn double_quoted(&mut self) -> Result<String> {
        let open = self.pos;
        self.pos += 1;
        let mut out = String::new();
        let mut keep = 0;
        loop {
            match self.peek() {
                _ if self.eof() => {
                    self.pos = open;
                    return Err(self.err("unterminated string"));
                }
                b'"' => {
                    self.pos += 1;
                    return Ok(out);
                }
                b'\\' => {
                    self.pos += 1;
                    if is_break(self.peek()) {
                        self.eat_break();
                        self.skip_blanks();
                    } else {
                        out.push(self.escape()?);
                    }
                }
                b'\n' | b'\r' => self.fold_quoted_break(&mut out, keep),
                c => {
                    let ch = self.next_char();
                    out.push(ch);
                    if is_blank(c) {
                        continue;
                    }
                }
            }
            keep = out.len();
        }
    }

    /// The escape after a `\`, which has been consumed.
    fn escape(&mut self) -> Result<char> {
        let c = self.peek();
        self.pos += 1;
        Ok(match c {
            b'0' => '\0',
            b'a' => '\x07',
            b'b' => '\x08',
            b't' | b'\t' => '\t',
            b'n' => '\n',
            b'v' => '\x0b',
            b'f' => '\x0c',
            b'r' => '\r',
            b'e' => '\x1b',
            b' ' => ' ',
            b'"' => '"',
            b'/' => '/',
            b'\\' => '\\',
            b'N' => '\u{85}',
            b'_' => '\u{a0}',
            b'L' => '\u{2028}',
            b'P' => '\u{2029}',
            b'x' => self.hex_char(2)?,
            b'u' => self.hex_char(4)?,
            b'U' => self.hex_char(8)?,
            _ => {
                self.pos -= 1;
                return Err(self.err("unknown escape"));
            }
        })
    }

    fn hex(&mut self, digits: usize) -> Result<u32> {
        let s = self.src.get(self.pos..self.pos + digits).unwrap_or("");
        if s.len() != digits || !s.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Err(self.err("bad hex digits in escape"));
        }
        self.pos += digits;
        Ok(u32::from_str_radix(s, 16).unwrap_or(0))
    }

    fn hex_char(&mut self, digits: usize) -> Result<char> {
        let mut code = self.hex(digits)?;
        // JSON spells astral characters as a \uXXXX surrogate pair.
        if digits == 4 && (0xd800..0xdc00).contains(&code) {
            if !self.b[self.pos..].starts_with(b"\\u") {
                return Err(self.err("unpaired surrogate in escape"));
            }
            self.pos += 2;
            let low = self.hex(4)?;
            if !(0xdc00..0xe000).contains(&low) {
                return Err(self.err("unpaired surrogate in escape"));
            }
            code = 0x10000 + ((code - 0xd800) << 10) + (low - 0xdc00);
        }
        char::from_u32(code).ok_or_else(|| self.err("escape is not a valid character"))
    }

    /// Skip blanks, line breaks and comments between flow tokens.
    fn flow_space(&mut self) -> Result<()> {
        loop {
            match self.peek() {
                b' ' | b'\t' | b'\n' | b'\r' => self.pos += 1,
                b'#' => {
                    if !self.comment_allowed() {
                        return Err(self.err("a comment needs whitespace before the '#'"));
                    }
                    self.skip_to_break();
                }
                _ => return Ok(()),
            }
        }
    }

    fn flow_node(&mut self) -> Result<Value> {
        match self.peek() {
            b'[' => {
                let open = self.pos;
                self.pos += 1;
                let mut seq = Vec::new();
                loop {
                    self.flow_space()?;
                    if self.eof() {
                        self.pos = open;
                        return Err(self.err("unclosed '['"));
                    }
                    if self.peek() == b']' {
                        self.pos += 1;
                        return Ok(Value::Seq(seq));
                    }
                    seq.push(self.flow_node()?);
                    self.flow_space()?;
                    match self.peek() {
                        b',' => self.pos += 1,
                        b']' => {}
                        _ if self.eof() => {}
                        _ => return Err(self.err("expected ',' or ']'")),
                    }
                }
            }
            b'{' => {
                let open = self.pos;
                self.pos += 1;
                let mut map: Vec<(String, Value)> = Vec::new();
                loop {
                    self.flow_space()?;
                    if self.eof() {
                        self.pos = open;
                        return Err(self.err("unclosed '{'"));
                    }
                    if self.peek() == b'}' {
                        self.pos += 1;
                        return Ok(Value::Map(map));
                    }
                    let key_pos = self.pos;
                    if matches!(self.peek(), b'[' | b'{') {
                        return Err(self.err("flow collections as keys are not supported"));
                    }
                    let key = self.scalar(true)?;
                    if matches!(key, Scalar::Plain("")) {
                        return Err(self.err("expected a mapping key"));
                    }
                    let key = key.into_key();
                    if map.iter().any(|(k, _)| *k == key) {
                        self.pos = key_pos;
                        return Err(self.err("duplicate key"));
                    }
                    self.flow_space()?;
                    let mut value = Value::Null;
                    if self.peek() == b':' {
                        self.pos += 1;
                        self.flow_space()?;
                        if !matches!(self.peek(), b',' | b'}') && !self.eof() {
                            value = self.flow_node()?;
                        }
                    }
                    map.push((key, value));
                    self.flow_space()?;
                    match self.peek() {
                        b',' => self.pos += 1,
                        b'}' => {}
                        _ if self.eof() => {}
                        _ => return Err(self.err("expected ',' or '}'")),
                    }
                }
            }
            _ => {
                let scalar = self.scalar(true)?;
                if matches!(scalar, Scalar::Plain("")) {
                    return Err(self.err("expected a value"));
                }
                Ok(scalar.into_value())
            }
        }
    }
}

/// Fold the lines of a `>` block scalar: a break between two plain lines is a
/// space, each empty line a newline, and breaks around more-indented lines
/// are kept as they are.
fn fold(lines: &[&str]) -> String {
    let mut out = String::new();
    let mut empties = 0;
    let mut prev_more = None;
    for line in lines {
        if line.is_empty() {
            empties += 1;
            continue;
        }
        let more = line.starts_with([' ', '\t']);
        match prev_more {
            None => out.extend(std::iter::repeat_n('\n', empties)),
            Some(false) if !more && empties == 0 => out.push(' '),
            Some(false) if !more => out.extend(std::iter::repeat_n('\n', empties)),
            Some(_) => out.extend(std::iter::repeat_n('\n', empties + 1)),
        }
        out.push_str(line);
        prev_more = Some(more);
        empties = 0;
    }
    out
}

/// Type a plain scalar per the YAML 1.2 core schema.
fn resolve_plain(s: &str) -> Value {
    match s {
        "" | "~" | "null" | "Null" | "NULL" => return Value::Null,
        "true" | "True" | "TRUE" => return Value::Bool(true),
        "false" | "False" | "FALSE" => return Value::Bool(false),
        ".inf" | ".Inf" | ".INF" | "+.inf" | "+.Inf" | "+.INF" => {
            return Value::Float(f64::INFINITY);
        }
        "-.inf" | "-.Inf" | "-.INF" => return Value::Float(f64::NEG_INFINITY),
        ".nan" | ".NaN" | ".NAN" => return Value::Float(f64::NAN),
        _ => {}
    }
    if let Some(i) = parse_int(s) {
        return Value::Int(i);
    }
    if is_float(s)
        && let Ok(f) = s.parse()
    {
        return Value::Float(f);
    }
    Value::String(s.to_string())
}

fn parse_int(s: &str) -> Option<i64> {
    let radix = |digits: &str, radix: u32| {
        if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
            return None;
        }
        i64::from_str_radix(digits, radix).ok()
    };
    if let Some(h) = s.strip_prefix("0x") {
        return radix(h, 16);
    }
    if let Some(o) = s.strip_prefix("0o") {
        return radix(o, 8);
    }
    let digits = s.strip_prefix(['-', '+']).unwrap_or(s);
    if digits.is_empty() || !digits.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// `[-+]? ( \. [0-9]+ | [0-9]+ ( \. [0-9]* )? ) ( [eE] [-+]? [0-9]+ )?`
fn is_float(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    let digits = |i: &mut usize| {
        let start = *i;
        while *i < b.len() && b[*i].is_ascii_digit() {
            *i += 1;
        }
        *i - start
    };
    if i < b.len() && (b[i] == b'-' || b[i] == b'+') {
        i += 1;
    }
    let int = digits(&mut i);
    if i < b.len() && b[i] == b'.' {
        i += 1;
        if digits(&mut i) == 0 && int == 0 {
            return false;
        }
    } else if int == 0 {
        return false;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        i += 1;
        if i < b.len() && (b[i] == b'-' || b[i] == b'+') {
            i += 1;
        }
        if digits(&mut i) == 0 {
            return false;
        }
    }
    i == b.len()
}
