//! JSON written the way the reference's `json.dumps` writes it.
//!
//! Sandbox memory files are read by a model and, across implementations, by the other side's
//! code, so their shape follows the reference: every character outside ASCII escaped as `\uXXXX`
//! (surrogate pairs beyond the basic plane), and the separators and indentation of the call that
//! wrote them. Field order is the serializer's: a struct writes its fields in declaration order,
//! which is how the reference's insertion-ordered dictionaries come out, and a
//! [`serde_json::Value`] object writes its keys sorted, which is `sort_keys=True`.

use std::io;
use std::time::SystemTime;

use ra_core::sandbox::events::format_event_timestamp;
use serde::Serialize;
use serde_json::Value;
use serde_json::ser::{Formatter, PrettyFormatter, Serializer};

/// Writes a run of characters that needs no escape of JSON's own, escaping what is not ASCII.
fn write_ascii_fragment<W: ?Sized + io::Write>(writer: &mut W, fragment: &str) -> io::Result<()> {
    let mut start = 0;
    for (index, character) in fragment.char_indices() {
        if character.is_ascii() {
            continue;
        }
        writer.write_all(&fragment.as_bytes()[start..index])?;
        let mut units = [0_u16; 2];
        for unit in character.encode_utf16(&mut units) {
            write!(writer, "\\u{unit:04x}")?;
        }
        start = index + character.len_utf8();
    }
    writer.write_all(&fragment.as_bytes()[start..])
}

/// `separators=(",", ":")`, no indentation: the trait's own defaults are the compact form.
struct AsciiCompact;

impl Formatter for AsciiCompact {
    fn write_string_fragment<W: ?Sized + io::Write>(
        &mut self,
        writer: &mut W,
        fragment: &str,
    ) -> io::Result<()> {
        write_ascii_fragment(writer, fragment)
    }
}

/// `indent=2` with the given key separator.
struct AsciiIndented {
    inner: PrettyFormatter<'static>,
    colon: &'static [u8],
}

impl Formatter for AsciiIndented {
    fn begin_array<W: ?Sized + io::Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.inner.begin_array(writer)
    }

    fn end_array<W: ?Sized + io::Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.inner.end_array(writer)
    }

    fn begin_array_value<W: ?Sized + io::Write>(
        &mut self,
        writer: &mut W,
        first: bool,
    ) -> io::Result<()> {
        self.inner.begin_array_value(writer, first)
    }

    fn end_array_value<W: ?Sized + io::Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.inner.end_array_value(writer)
    }

    fn begin_object<W: ?Sized + io::Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.inner.begin_object(writer)
    }

    fn end_object<W: ?Sized + io::Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.inner.end_object(writer)
    }

    fn begin_object_key<W: ?Sized + io::Write>(
        &mut self,
        writer: &mut W,
        first: bool,
    ) -> io::Result<()> {
        self.inner.begin_object_key(writer, first)
    }

    fn begin_object_value<W: ?Sized + io::Write>(&mut self, writer: &mut W) -> io::Result<()> {
        writer.write_all(self.colon)
    }

    fn end_object_value<W: ?Sized + io::Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.inner.end_object_value(writer)
    }

    fn write_string_fragment<W: ?Sized + io::Write>(
        &mut self,
        writer: &mut W,
        fragment: &str,
    ) -> io::Result<()> {
        write_ascii_fragment(writer, fragment)
    }
}

fn serialize_with<T: Serialize + ?Sized, F: Formatter>(
    value: &T,
    formatter: F,
) -> Result<String, serde_json::Error> {
    let mut out = Vec::new();
    let mut serializer = Serializer::with_formatter(&mut out, formatter);
    value.serialize(&mut serializer)?;
    String::from_utf8(out).map_err(|error| serde_json::Error::io(io::Error::other(error)))
}

/// `json.dumps(value, separators=(",", ":"))`.
pub(crate) fn dumps_compact<T: Serialize + ?Sized>(value: &T) -> Result<String, serde_json::Error> {
    serialize_with(value, AsciiCompact)
}

/// `json.dumps(value, indent=2, separators=(",", ":"))`.
pub(crate) fn dumps_indented_tight<T: Serialize + ?Sized>(
    value: &T,
) -> Result<String, serde_json::Error> {
    serialize_with(
        value,
        AsciiIndented {
            inner: PrettyFormatter::with_indent(b"  "),
            colon: b":",
        },
    )
}

/// `json.dumps(value, indent=2)`, whose key separator keeps its space.
pub(crate) fn dumps_indented<T: Serialize + ?Sized>(
    value: &T,
) -> Result<String, serde_json::Error> {
    serialize_with(
        value,
        AsciiIndented {
            inner: PrettyFormatter::with_indent(b"  "),
            colon: b": ",
        },
    )
}

/// `datetime.now(tz=timezone.utc).isoformat()` for `time`: microseconds when there are any, and a
/// `+00:00` offset.
pub(crate) fn utc_isoformat(time: SystemTime) -> String {
    let mut text = format_event_timestamp(time);
    text.pop();
    text.push_str("+00:00");
    text
}

/// Quotes a string as Python's `repr` does, for messages the reference words that way.
pub(crate) fn python_repr(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(text.len() + 2);
    out.push(quote);
    for character in text.chars() {
        match character {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// Python's `str(value)` for a JSON value that is truthy, or `None` for one that is not — the
/// reference's `str(value or default)` with the default left to the caller.
pub(crate) fn python_truthy_str(value: &Value) -> Option<String> {
    match value {
        Value::Null | Value::Bool(false) => None,
        Value::Bool(true) => Some("True".to_owned()),
        Value::String(text) if text.is_empty() => None,
        Value::String(text) => Some(text.clone()),
        Value::Number(number) if number.as_f64() == Some(0.0) => None,
        Value::Number(number) => Some(number.to_string()),
        Value::Array(items) if items.is_empty() => None,
        Value::Object(fields) if fields.is_empty() => None,
        other => Some(other.to_string()),
    }
}

/// Python's `str.strip()`: Unicode whitespace and the four information separators.
pub(crate) fn python_strip(text: &str) -> &str {
    text.trim_matches(|character: char| {
        character.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&character)
    })
}

/// Python's `str.rstrip()`, the trailing half of [`python_strip`].
pub(crate) fn python_rstrip(text: &str) -> &str {
    text.trim_end_matches(|character: char| {
        character.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&character)
    })
}
