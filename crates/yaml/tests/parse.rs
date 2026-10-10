use yaml::{Error, Value, parse};

fn s(v: &str) -> Value {
    Value::String(v.to_string())
}

fn seq(items: Vec<Value>) -> Value {
    Value::Seq(items)
}

fn map(entries: Vec<(&str, Value)>) -> Value {
    Value::Map(entries.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

fn ok(text: &str) -> Value {
    parse(text).unwrap_or_else(|e| panic!("{e} in {text:?}"))
}

fn err(text: &str) -> Error {
    match parse(text) {
        Ok(v) => panic!("expected an error, got {v:?} from {text:?}"),
        Err(e) => e,
    }
}

// ---- documents ----

#[test]
fn an_empty_document_is_null() {
    assert_eq!(ok(""), Value::Null);
    assert_eq!(ok("\n\n"), Value::Null);
    assert_eq!(ok("# only a comment\n   # and another\n"), Value::Null);
    assert_eq!(ok("---\n"), Value::Null);
}

#[test]
fn document_markers_are_accepted_around_one_document() {
    assert_eq!(ok("---\na: 1\n"), map(vec![("a", Value::Int(1))]));
    assert_eq!(ok("# lead\n---\na: 1\n...\n# trail\n"), map(vec![("a", Value::Int(1))]));
    assert_eq!(ok("--- [1, 2]\n"), seq(vec![Value::Int(1), Value::Int(2)]));
    assert_eq!(ok("--- |\n  text\n"), s("text\n"));
}

#[test]
fn a_second_document_is_rejected() {
    let e = err("a: 1\n---\nb: 2\n");
    assert_eq!((e.line, e.msg), (2, "multiple documents are not supported"));
}

#[test]
fn a_bom_and_crlf_line_ends_are_tolerated() {
    assert_eq!(
        ok("\u{feff}a: 1\r\nb:\r\n  - x\r\n"),
        map(vec![("a", Value::Int(1)), ("b", seq(vec![s("x")]))])
    );
}

// ---- scalars ----

#[test]
fn plain_scalars_resolve_per_the_core_schema() {
    let cases = [
        ("~", Value::Null),
        ("null", Value::Null),
        ("NULL", Value::Null),
        ("true", Value::Bool(true)),
        ("False", Value::Bool(false)),
        ("42", Value::Int(42)),
        ("-7", Value::Int(-7)),
        ("+3", Value::Int(3)),
        ("0x1F", Value::Int(31)),
        ("0o17", Value::Int(15)),
        ("1.5", Value::Float(1.5)),
        ("-.5", Value::Float(-0.5)),
        ("1e3", Value::Float(1000.0)),
        ("2.", Value::Float(2.0)),
        (".inf", Value::Float(f64::INFINITY)),
        ("-.Inf", Value::Float(f64::NEG_INFINITY)),
        // YAML 1.1 booleans are plain strings under the core schema.
        ("yes", s("yes")),
        ("no", s("no")),
        ("on", s("on")),
        ("y", s("y")),
        ("1.2.3", s("1.2.3")),
        ("0x", s("0x")),
        ("1_000", s("1_000")),
        ("inf", s("inf")),
        ("Ctrl+Shift+Z", s("Ctrl+Shift+Z")),
        ("Ctrl+/", s("Ctrl+/")),
        ("-5x", s("-5x")),
    ];
    for (text, want) in cases {
        assert_eq!(ok(&format!("v: {text}\n")).get("v"), Some(&want), "{text}");
    }
    assert!(matches!(ok("v: .nan").get("v"), Some(Value::Float(f)) if f.is_nan()));
}

#[test]
fn a_plain_scalar_keeps_inner_colons_and_hashes() {
    assert_eq!(ok("url: http://x.y/a#b\n").get("url"), Some(&s("http://x.y/a#b")));
    assert_eq!(ok("t: 12:30\n").get("t"), Some(&s("12:30")));
    assert_eq!(ok("k: a b  c   \n").get("k"), Some(&s("a b  c")));
}

#[test]
fn a_comment_ends_a_plain_scalar() {
    assert_eq!(ok("k: value   # note\n").get("k"), Some(&s("value")));
    assert_eq!(ok("k:  # nothing here\n").get("k"), Some(&Value::Null));
}

#[test]
fn an_empty_value_is_null() {
    assert_eq!(ok("a:\nb: 1\n"), map(vec![("a", Value::Null), ("b", Value::Int(1))]));
    assert_eq!(ok("a:"), map(vec![("a", Value::Null)]));
}

#[test]
fn quoted_scalars_are_always_strings() {
    assert_eq!(
        ok("a: \"1\"\nb: 'true'\nc: \"\"\n"),
        map(vec![("a", s("1")), ("b", s("true")), ("c", s(""))])
    );
}

#[test]
fn single_quotes_escape_only_themselves() {
    assert_eq!(ok("a: 'it''s \\n'\n").get("a"), Some(&s("it's \\n")));
}

#[test]
fn double_quoted_escapes() {
    let v = ok(r#"a: "tab\there\nnl \"q\" \\ \/ \x41 \u00e9 \U0001F600 \ud83d\ude00 \0""#);
    assert_eq!(v.get("a"), Some(&s("tab\there\nnl \"q\" \\ / A \u{e9} \u{1F600} \u{1F600} \0")));
}

#[test]
fn bad_escapes_are_errors() {
    assert_eq!(err(r#"a: "\q""#).msg, "unknown escape");
    assert_eq!(err(r#"a: "\x4""#).msg, "bad hex digits in escape");
    assert_eq!(err(r#"a: "\ud83d""#).msg, "unpaired surrogate in escape");
}

#[test]
fn quoted_scalars_fold_across_lines() {
    assert_eq!(ok("a: \"one   \n   two\n\n   three\"\n").get("a"), Some(&s("one two\nthree")));
    assert_eq!(ok("a: 'one\n  two'\n").get("a"), Some(&s("one two")));
    assert_eq!(ok("a: \"one\\\n   two\"\n").get("a"), Some(&s("onetwo")));
    // An escaped tab survives the trim before a fold.
    assert_eq!(ok("a: \"x\\t\n  y\"\n").get("a"), Some(&s("x\t y")));
}

#[test]
fn an_unterminated_string_points_at_its_opening_quote() {
    let e = err("a: 1\nb: \"open\n");
    assert_eq!((e.line, e.col, e.msg), (2, 4, "unterminated string"));
}

#[test]
fn text_after_a_quoted_scalar_is_an_error() {
    assert_eq!(err("a: \"x\" y\n").msg, "unexpected text after the value");
    assert_eq!(err("a: \"x\"# c\n").msg, "a comment needs whitespace before the '#'");
}

// ---- block scalars ----

#[test]
fn literal_block_scalar_chomping() {
    let text = "clip: |\n  a\n   b\n\n  c\n\n\nstrip: |-\n  a\n\nkeep: |+\n  a\n\n\nnext: 1\n";
    assert_eq!(
        ok(text),
        map(vec![
            ("clip", s("a\n b\n\nc\n")),
            ("strip", s("a")),
            ("keep", s("a\n\n\n")),
            ("next", Value::Int(1)),
        ])
    );
}

#[test]
fn folded_block_scalar() {
    let text = ">\n  folded\n  line\n\n  next\n  line\n    more\n    indented\n  back\n";
    assert_eq!(ok(text), s("folded line\nnext line\n  more\n  indented\nback\n"));
}

#[test]
fn block_scalar_keeps_comments_and_quotes_as_text() {
    assert_eq!(
        ok("a: |\n  # not a comment\n  \"q\" 'q'\n").get("a"),
        Some(&s("# not a comment\n\"q\" 'q'\n"))
    );
}

#[test]
fn block_scalar_header_takes_indicators_in_either_order() {
    assert_eq!(ok("a: |2-\n    x\n").get("a"), Some(&s("  x")));
    assert_eq!(ok("a: |-2\n    x\n").get("a"), Some(&s("  x")));
    assert_eq!(ok("a: >+ # c\n  x\n\n").get("a"), Some(&s("x\n\n")));
}

#[test]
fn an_empty_block_scalar_is_an_empty_string() {
    assert_eq!(ok("a: |\nb: 1\n"), map(vec![("a", s("")), ("b", Value::Int(1))]));
}

#[test]
fn block_scalars_in_sequences() {
    assert_eq!(ok("- |\n  one\n- >-\n  two\n  three\n"), seq(vec![s("one\n"), s("two three")]));
}

// ---- block collections ----

#[test]
fn nested_block_mappings() {
    let text = "a:\n  b: 1\n  c:\n    d: x\ne: y\n";
    assert_eq!(
        ok(text),
        map(vec![
            ("a", map(vec![("b", Value::Int(1)), ("c", map(vec![("d", s("x"))]))])),
            ("e", s("y")),
        ])
    );
}

#[test]
fn a_scalar_value_may_sit_on_the_next_line() {
    assert_eq!(ok("a:\n  value\n"), map(vec![("a", s("value"))]));
}

#[test]
fn keys_may_hold_spaces_and_be_quoted() {
    assert_eq!(
        ok("select all: x\n\"*.md\": markdown\n'k: v': 1\n"),
        map(vec![("select all", s("x")), ("*.md", s("markdown")), ("k: v", Value::Int(1))])
    );
}

#[test]
fn sequences_indented_or_at_their_keys_column() {
    let want = map(vec![("a", seq(vec![s("x"), s("y")])), ("b", Value::Int(1))]);
    assert_eq!(ok("a:\n  - x\n  - y\nb: 1\n"), want);
    assert_eq!(ok("a:\n- x\n- y\nb: 1\n"), want);
}

#[test]
fn mappings_inside_sequences() {
    let text = "- name: a\n  lang: x\n-\n  name: b\n- - p\n  - q\n-\n";
    assert_eq!(
        ok(text),
        seq(vec![
            map(vec![("name", s("a")), ("lang", s("x"))]),
            map(vec![("name", s("b"))]),
            seq(vec![s("p"), s("q")]),
            Value::Null,
        ])
    );
}

#[test]
fn comment_lines_may_sit_at_any_indentation() {
    assert_eq!(
        ok("a:\n# x\n      # y\n  b: 1\n # z\nc: 2\n"),
        map(vec![("a", map(vec![("b", Value::Int(1))])), ("c", Value::Int(2))])
    );
}

#[test]
fn a_root_mapping_may_be_indented() {
    assert_eq!(ok("  a: 1\n  b: 2\n"), map(vec![("a", Value::Int(1)), ("b", Value::Int(2))]));
}

#[test]
fn a_root_scalar() {
    assert_eq!(ok("hello world\n"), s("hello world"));
    assert_eq!(ok("'quoted'"), s("quoted"));
}

// ---- flow collections ----

#[test]
fn json_is_yaml() {
    let text = r#"{
  "name": "edit",
  "tags": ["a", "b"],
  "n": -1.5e3,
  "ok": true,
  "none": null,
  "nested": {"empty": [], "obj": {}}
}"#;
    assert_eq!(
        ok(text),
        map(vec![
            ("name", s("edit")),
            ("tags", seq(vec![s("a"), s("b")])),
            ("n", Value::Float(-1500.0)),
            ("ok", Value::Bool(true)),
            ("none", Value::Null),
            ("nested", map(vec![("empty", seq(vec![])), ("obj", map(vec![]))])),
        ])
    );
}

#[test]
fn flow_collections_take_plain_scalars_and_trailing_commas() {
    assert_eq!(ok("a: [Ctrl+Q, Ctrl+W,]\n").get("a"), Some(&seq(vec![s("Ctrl+Q"), s("Ctrl+W")])));
    assert_eq!(
        ok("a: {x: 1, y, z: }\n").get("a"),
        Some(&map(vec![("x", Value::Int(1)), ("y", Value::Null), ("z", Value::Null)]))
    );
    assert_eq!(ok("[a b, -1]").as_seq(), Some(&[s("a b"), Value::Int(-1)][..]));
}

#[test]
fn flow_collections_span_lines_with_comments() {
    let text = "exit: [\n  \"Ctrl+Q\",  # first\n  \"Ctrl+W\",\n]\nsave: x\n";
    assert_eq!(
        ok(text),
        map(vec![("exit", seq(vec![s("Ctrl+Q"), s("Ctrl+W")])), ("save", s("x"))])
    );
}

#[test]
fn flow_errors() {
    assert_eq!(err("a: [1, 2\n").msg, "unclosed '['");
    assert_eq!(err("a: {b: 1\n").msg, "unclosed '{'");
    assert_eq!(err("a: [1 2: 3]\n").msg, "expected ',' or ']'");
    assert_eq!(err("a: [1,,2]\n").msg, "unexpected flow indicator");
    assert_eq!(err("a: [1] x\n").msg, "unexpected text after the value");
    assert_eq!(err("a: {b: 1, b: 2}\n").msg, "duplicate key");
}

// ---- rejected features and malformed input ----

#[test]
fn unsupported_features_say_so() {
    assert_eq!(err("a: &x 1\n").msg, "anchors are not supported");
    assert_eq!(err("a: *x\n").msg, "aliases are not supported; quote a value that starts with '*'");
    assert_eq!(
        err("*.md: markdown\n").msg,
        "aliases are not supported; quote a value that starts with '*'"
    );
    assert_eq!(err("a: !!str 1\n").msg, "tags are not supported");
    assert_eq!(
        err("%YAML 1.2\n---\na: 1\n").msg,
        "reserved character; quote a value that starts with it"
    );
    assert_eq!(err("? a\n: b\n").msg, "complex keys are not supported");
    assert_eq!(err("[a]: 1\n").msg, "unexpected text after the value");
}

#[test]
fn a_multi_line_plain_scalar_is_rejected() {
    assert_eq!(err("a: one\n  two\n").msg, "unexpected indentation");
}

#[test]
fn duplicate_keys_point_at_the_second() {
    let e = err("a: 1\nb: 2\na: 3\n");
    assert_eq!((e.line, e.col, e.msg), (3, 1, "duplicate key"));
}

#[test]
fn indentation_errors() {
    assert_eq!(err("a: 1\n  b: 2\n").msg, "unexpected indentation");
    assert_eq!(err("- a\n   - b\n").msg, "unexpected indentation");
    assert_eq!(err("a:\n  b: 1\n c: 2\n").msg, "unexpected indentation");
    assert_eq!(err("  a: 1\nb: 2\n").msg, "unexpected content after the document");
    let e = err("a:\n\tb: 1\n");
    assert_eq!((e.line, e.col, e.msg), (2, 1, "tabs are not allowed in indentation"));
}

#[test]
fn a_lone_dash_is_a_sequence_entry_not_a_scalar() {
    assert_eq!(err("v: -\n").msg, "a block sequence must start on its own line");
    assert_eq!(ok("v: '-'\n").get("v"), Some(&s("-")));
}

#[test]
fn collections_cannot_start_on_a_keys_line() {
    assert_eq!(err("a: b: c\n").msg, "a nested mapping must start on its own line");
    assert_eq!(err("a: - b\n").msg, "a block sequence must start on its own line");
}

#[test]
fn a_mapping_entry_needs_a_colon() {
    assert_eq!(err("a: 1\nb\n").msg, "expected ':' after the key");
    assert_eq!(err("a: 1\n- b\n").msg, "unexpected block sequence entry");
}

#[test]
fn error_columns_count_characters() {
    let e = err("k: '\u{e9}\u{e9}' x\n");
    assert_eq!((e.line, e.col), (1, 9));
    assert_eq!(e.to_string(), "1:9: unexpected text after the value");
}

// ---- accessors ----

#[test]
fn accessors() {
    let v = ok("a: x\nb: [1]\nc: {d: 2}\n");
    assert_eq!(v.get("a").and_then(Value::as_str), Some("x"));
    assert_eq!(v.get("b").and_then(Value::as_seq), Some(&[Value::Int(1)][..]));
    assert_eq!(v.get("c").and_then(|c| c.get("d")), Some(&Value::Int(2)));
    assert_eq!(v.get("missing"), None);
    assert_eq!(v.get("a").and_then(Value::as_map), None);
    assert_eq!(s("x").get("a"), None);
}

#[test]
fn a_keybindings_file_round_trips() {
    let text = "\
# Edit keybindings.
keybindings:
  # Ctrl even on macOS.
  exit: [Ctrl+Q, Ctrl+W]
  save: Cmd+S
  focus_statusbar: \"\"
  open_about:
  toggle_line_comment: Cmd+/
";
    let kb = ok(text);
    let kb = kb.get("keybindings").unwrap();
    assert_eq!(kb.get("exit"), Some(&seq(vec![s("Ctrl+Q"), s("Ctrl+W")])));
    assert_eq!(kb.get("save"), Some(&s("Cmd+S")));
    assert_eq!(kb.get("focus_statusbar"), Some(&s("")));
    assert_eq!(kb.get("open_about"), Some(&Value::Null));
    assert_eq!(kb.get("toggle_line_comment"), Some(&s("Cmd+/")));
}
