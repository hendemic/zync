//! Editing a config file in place, one line at a time.
//!
//! A serde round-trip would be four lines of code, but it reflows the document
//! and drops every comment in it — including the annotations the example config
//! ships with and whatever the user has added since. So the new values are
//! spliced into the existing text instead: a changed scalar has its value
//! rewritten between the `key:` and any trailing comment, and everything else
//! in the file is copied through byte for byte.
//!
//! Only the block style the example config uses is understood: a top-level
//! mapping, two-space nesting, one `key: value` per line, `#` comments. Anything
//! else makes [`splice`] bail, and the caller falls back to a full rendering.
//! Nothing here touches the filesystem, and nothing here decides whether the
//! result is good enough to write — [`super::save_to`] verifies that.

use anyhow::{Result, bail};
use serde::Serialize;
use serde_yaml::Value;
use zync_core::domain::{Config, Intensity};

/// Nesting step the example config uses, and the fallback when a block being
/// replaced has no indented line to copy it from.
const INDENT: usize = 2;

/// Rewrites `existing` so that it parses back to `config`, keeping as much of the
/// original text as it can.
///
/// Comments inside the `lights:` and `zones:` blocks are lost when those blocks
/// change, because a list of mappings is rewritten whole rather than field by
/// field. Every other comment, including the ones between blocks, survives.
pub(super) fn splice(existing: &str, config: &Config) -> Result<String> {
    // A document that is not a plain mapping is one none of the line handling
    // below would read correctly, so rule it out before editing anything.
    if serde_yaml::from_str::<serde_yaml::Mapping>(existing).is_err() {
        bail!("the file is not a top-level YAML mapping");
    }

    let mut lines: Vec<String> = existing.lines().map(str::to_owned).collect();
    let entries = top_level_entries(&lines)?;

    let mut patches = Vec::new();
    let mut appended = Vec::new();
    let mut trim_trailing_blanks = false;

    for (key, plan) in plans(config)? {
        match entries.iter().find(|entry| entry.key == key) {
            Some(entry) => {
                let Some(replacement) = patch(&lines, entry, &plan)? else {
                    continue;
                };
                // An emptied top-level entry at the end of the file would leave
                // the blank line that was separating it behind.
                let mut start = entry.start;
                let mut end = entry.end;
                if replacement.is_empty() && lines[end..].iter().all(|line| line.trim().is_empty()) {
                    end = lines.len();
                    while start > 0 && lines[start - 1].trim().is_empty() {
                        start -= 1;
                    }
                    trim_trailing_blanks = true;
                }
                patches.push((start, end, replacement));
            }
            None => appended.extend(new_entry(key, &plan)?),
        }
    }

    // Bottom-up, so a patch never has to have its range adjusted for the ones
    // applied before it. Top-level entries do not overlap, so order is enough.
    patches.sort_by_key(|(start, ..)| std::cmp::Reverse(*start));
    for (start, end, replacement) in patches {
        lines.splice(start..end, replacement);
    }

    if !appended.is_empty() {
        if lines.last().is_some_and(|line| !line.trim().is_empty()) {
            lines.push(String::new());
        }
        lines.extend(appended);
    }
    if trim_trailing_blanks {
        while lines.last().is_some_and(|line| line.trim().is_empty()) {
            lines.pop();
        }
    }

    let mut out = lines.join("\n");
    // A config file ends in a newline; `lines()` has already dropped the one the
    // original had.
    if !out.is_empty() {
        out.push('\n');
    }

    Ok(out)
}

/// One top-level key and the lines that belong to it.
///
/// `end` is exclusive and excludes trailing blank lines, so that a value
/// appended to the block lands against the block's last line rather than after
/// the blank line that separates it from the next key.
struct Entry {
    key: String,
    start: usize,
    end: usize,
}

/// What a key's new text looks like, and how much of the document it takes up.
enum Shape {
    /// `key: value` on a single line. `quoted` marks the free-text strings: those
    /// are written in double quotes, the way every string in the example config
    /// is, while an enum name or a number is left bare.
    Line { text: String, value: Value, quoted: bool },
    /// `key:` followed by an indented rendering of the value.
    Block { body: String, value: Value },
    /// The key does not appear at all, because its value is `None`.
    Absent,
}

/// How one top-level key is written: directly, or as a mapping whose fields are
/// edited one at a time so the comments between them survive.
enum Plan {
    Top(Shape),
    Nested {
        /// The mapping as a whole, for deciding whether anything changed at all.
        value: Value,
        fields: Vec<(&'static str, Shape)>,
    },
}

/// Every field of a config, paired with the shape it takes on disk.
///
/// The order here is the order missing keys are appended in, so it follows the
/// order of the example config rather than the order of the struct.
fn plans(config: &Config) -> Result<Vec<(&'static str, Plan)>> {
    let mqtt = &config.mqtt;
    let performance = &config.performance;

    Ok(vec![
        (
            "mqtt",
            Plan::Nested {
                value: to_value(mqtt)?,
                fields: vec![
                    ("name", text(&mqtt.name)?),
                    ("broker", text(&mqtt.broker)?),
                    ("port", line(&mqtt.port)?),
                    ("user", optional_text(mqtt.user.as_ref())?),
                    ("password", optional_text(mqtt.password.as_ref())?),
                ],
            },
        ),
        ("downsample_factor", Plan::Top(line(&config.downsample_factor)?)),
        ("instance", Plan::Top(optional_text(config.instance.as_ref())?)),
        ("on_stop", Plan::Top(line(&config.on_stop)?)),
        (
            "intensity",
            // The one field that is a scalar or a block depending on its value.
            Plan::Top(match config.intensity {
                Intensity::Custom(_) => block(&config.intensity)?,
                _ => line(&config.intensity)?,
            }),
        ),
        ("lights", Plan::Top(block(&config.lights)?)),
        ("zones", Plan::Top(block(&config.zones)?)),
        (
            "performance",
            Plan::Nested {
                value: to_value(performance)?,
                fields: vec![
                    ("max_fps", line(&performance.max_fps)?),
                    ("max_delay", line(&performance.max_delay)?),
                    ("refresh_threshold", line(&performance.refresh_threshold)?),
                    ("percent_thread_work", line(&performance.percent_thread_work)?),
                    ("fps_reporting", line(&performance.fps_reporting)?),
                    ("max_commands_per_sec", line(&performance.max_commands_per_sec)?),
                ],
            },
        ),
    ])
}

/// The value's YAML, rendered by the writer that reads f32 as f32: going through
/// `serde_yaml::Value` first would widen `0.1` to its f64 expansion.
fn rendered<T: Serialize>(value: &T) -> Result<String> {
    Ok(serde_yaml::to_string(value)?)
}

/// Reparsed rather than converted, so that the value compared against the file is
/// exactly the one the written text will read back as.
fn to_value<T: Serialize>(value: &T) -> Result<Value> {
    Ok(serde_yaml::from_str(&rendered(value)?)?)
}

fn line<T: Serialize>(value: &T) -> Result<Shape> {
    scalar(value, false)
}

/// A field whose value is free text the user typed.
fn text(value: &str) -> Result<Shape> {
    scalar(&value, true)
}

fn optional_text(value: Option<&String>) -> Result<Shape> {
    value.map_or(Ok(Shape::Absent), |value| text(value))
}

fn scalar<T: Serialize>(value: &T, quoted: bool) -> Result<Shape> {
    let rendered = rendered(value)?;
    let text = rendered.trim_end_matches('\n');
    if text.contains('\n') {
        bail!("a value that does not fit on one line: {text}");
    }

    Ok(Shape::Line { text: text.to_owned(), value: to_value(value)?, quoted })
}

fn block<T: Serialize>(value: &T) -> Result<Shape> {
    Ok(Shape::Block { body: rendered(value)?, value: to_value(value)? })
}

/// Splits the document into its top-level keys.
///
/// A key owns every line after it that is blank or indented; the first line at
/// column 0 that is not — the next key, or a comment introducing it — ends the
/// block. Anything else at column 0, and any indented line that belongs to no
/// key, is a shape this module does not understand.
fn top_level_entries(lines: &[String]) -> Result<Vec<Entry>> {
    let mut entries: Vec<Entry> = Vec::new();
    let mut index = 0;

    while index < lines.len() {
        let line = &lines[index];
        if line.trim().is_empty() || line.starts_with('#') {
            index += 1;
            continue;
        }
        if line.starts_with([' ', '\t']) {
            bail!("line {}: indented under no key", index + 1);
        }

        let Some(key) = key_of(line) else {
            bail!("line {}: not a `key: value` line", index + 1);
        };
        if entries.iter().any(|entry| entry.key == key) {
            bail!("line {}: '{key}' appears twice", index + 1);
        }

        let mut end = index + 1;
        while end < lines.len()
            && (lines[end].trim().is_empty() || lines[end].starts_with([' ', '\t']))
        {
            end += 1;
        }
        let content_end = lines[..end]
            .iter()
            .rposition(|line| !line.trim().is_empty())
            .map_or(index + 1, |last| last + 1);

        entries.push(Entry { key: key.to_owned(), start: index, end: content_end });
        index = end;
    }

    Ok(entries)
}

/// The key a mapping line declares, or `None` for anything that is not one.
fn key_of(line: &str) -> Option<&str> {
    let key = line.trim_start();
    let indent = line.len() - key.len();
    let colon = key.find(':')?;
    let (key, rest) = key.split_at(colon);

    let named = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.');
    let starts_well = key.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_');
    let followed_by_space = rest[1..].starts_with(char::is_whitespace) || rest.len() == 1;

    (starts_well && key.chars().all(named) && followed_by_space)
        .then(|| &line[indent..indent + key.len()])
}

/// The replacement lines for an entry that is already in the document, or `None`
/// when it needs no change.
fn patch(lines: &[String], entry: &Entry, plan: &Plan) -> Result<Option<Vec<String>>> {
    let region = &lines[entry.start..entry.end];
    let current = value_of(region, &entry.key)?;

    match plan {
        Plan::Top(shape) => {
            if same_value(current.as_ref(), shape_value(shape)) {
                return Ok(None);
            }
            Ok(Some(written(entry, region, shape)))
        }
        Plan::Nested { value, fields } => {
            if same_value(current.as_ref(), Some(value)) {
                return Ok(None);
            }
            Ok(Some(nested(region, fields)?))
        }
    }
}

/// The entry's own value, as the file currently spells it. `None` when the key is
/// there with nothing after it.
fn value_of(region: &[String], key: &str) -> Result<Option<Value>> {
    let text = region.join("\n");
    let mapping: serde_yaml::Mapping = serde_yaml::from_str(&text)?;

    Ok(mapping
        .get(Value::String(key.to_owned()))
        .filter(|value| !value.is_null())
        .cloned())
}

fn shape_value(shape: &Shape) -> Option<&Value> {
    match shape {
        Shape::Line { value, .. } | Shape::Block { value, .. } => Some(value),
        Shape::Absent => None,
    }
}

/// Numbers compare by their value, because YAML spells the same one as `6` or
/// `6.0` and a difference in spelling must not provoke a rewrite.
fn same_value(current: Option<&Value>, wanted: Option<&Value>) -> bool {
    match (current, wanted) {
        (None, None) => true,
        (Some(Value::Number(a)), Some(Value::Number(b))) => match (a.as_f64(), b.as_f64()) {
            (Some(a), Some(b)) => a == b,
            _ => a == b,
        },
        (Some(Value::Sequence(a)), Some(Value::Sequence(b))) => {
            a.len() == b.len()
                && a.iter()
                    .zip(b)
                    .all(|(a, b)| same_value(Some(a), Some(b)))
        }
        (Some(Value::Mapping(a)), Some(Value::Mapping(b))) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(key, a)| same_value(Some(a), b.get(key)))
        }
        (a, b) => a == b,
    }
}

/// The lines a top-level entry becomes, keeping its trailing comment where the
/// new value still fits on one line as the old one did.
fn written(entry: &Entry, region: &[String], shape: &Shape) -> Vec<String> {
    let single_line = region.len() == 1;

    match shape {
        Shape::Absent => Vec::new(),
        Shape::Line { text, quoted, .. } if single_line => {
            vec![rewritten(&region[0], &entry.key, text, *quoted)]
        }
        // Collapsed from a block, so there is no line to keep the shape of.
        Shape::Line { text, quoted, .. } => {
            vec![format!("{}: {}", entry.key, appended(text, *quoted))]
        }
        Shape::Block { body, .. } => {
            let indent = region
                .iter()
                .skip(1)
                .find(|line| !line.trim().is_empty())
                .map_or(INDENT, |line| line.len() - line.trim_start().len());

            std::iter::once(format!("{}:", entry.key))
                .chain(indented(body, indent))
                .collect()
        }
    }
}

/// A rendered value, pushed in under the key it belongs to.
fn indented(body: &str, indent: usize) -> impl Iterator<Item = String> {
    let pad = " ".repeat(indent);
    body.lines().map(move |line| match line.is_empty() {
        true => String::new(),
        false => format!("{pad}{line}"),
    })
}

/// A mapping block, edited field by field so that everything between the fields
/// — comments, blank lines, keys this config does not know about — is kept.
fn nested(region: &[String], fields: &[(&str, Shape)]) -> Result<Vec<String>> {
    let child_indent = region
        .iter()
        .skip(1)
        .find(|line| !line.trim().is_empty())
        .map_or(INDENT, |line| line.len() - line.trim_start().len());

    let mut out: Vec<String> = region.to_vec();

    // Highest index first, so removing a line cannot shift the ones still to do.
    let mut edits: Vec<(usize, Option<String>)> = Vec::new();
    let mut additions: Vec<String> = Vec::new();

    for (key, shape) in fields {
        let found = region
            .iter()
            .skip(1)
            .position(|line| is_field(line, key))
            .map(|offset| offset + 1);

        match (found, shape) {
            (Some(index), Shape::Line { text, value, quoted }) => {
                let existing = field_value(&region[index], key)
                    .map(serde_yaml::from_str::<Value>)
                    .transpose()?;
                if !same_value(existing.as_ref(), Some(value)) {
                    edits.push((index, Some(rewritten(&region[index], key, text, *quoted))));
                }
            }
            (Some(index), Shape::Absent) => edits.push((index, None)),
            // A field an older config never had, indented to match its siblings.
            (None, Shape::Line { text, quoted, .. }) => additions.push(format!(
                "{}{key}: {}",
                " ".repeat(child_indent),
                appended(text, *quoted)
            )),
            (None, Shape::Absent) => (),
            (_, Shape::Block { .. }) => bail!("'{key}' is a block inside a mapping field"),
        }
    }

    edits.sort_by_key(|(index, _)| std::cmp::Reverse(*index));
    for (index, replacement) in edits {
        match replacement {
            Some(line) => out[index] = line,
            None => {
                out.remove(index);
            }
        }
    }
    out.extend(additions);

    Ok(out)
}

/// Whether a line inside a block declares `key`. Column is not checked beyond
/// being indented at all: the two mappings edited this way have no nesting of
/// their own for a deeper `key:` to hide in.
fn is_field(line: &str, key: &str) -> bool {
    line.starts_with([' ', '\t']) && key_of(line) == Some(key)
}

/// Replaces the value on a `key: value` line, leaving the key, the indentation,
/// the spacing and any trailing comment as they were.
fn rewritten(line: &str, key: &str, text: &str, quoted: bool) -> String {
    let Some((head, old, trail, comment)) = parts(line, key) else {
        // Not a shape `parts` reads, which only happens if the caller matched a
        // line this did not; rebuild it from the key rather than corrupt it.
        return format!("{key}: {text}");
    };

    // A line with no value at all leaves no space to put one after.
    let head = match old.is_empty() && !head.ends_with(' ') {
        true => format!("{head} "),
        false => head.to_owned(),
    };
    let text = quoted
        .then(|| quoted_like(old, text))
        .flatten()
        .unwrap_or_else(|| text.to_owned());
    // Keeping the comment in its column is worth a little padding: the example
    // config aligns them, and a changed number should not pull one out of line.
    let trail = match comment.is_empty() {
        true => trail.to_owned(),
        false => " ".repeat((old.len() + trail.len()).saturating_sub(text.len()).max(1)),
    };

    format!("{head}{text}{trail}{comment}")
}

/// A value being written on a line that did not exist before, so there is no
/// existing quoting to follow — only the file's prevailing style, which quotes
/// every string it has.
fn appended(text: &str, quoted: bool) -> String {
    quoted
        .then(|| quoted_like("\"\"", text))
        .flatten()
        .unwrap_or_else(|| text.to_owned())
}

/// The value text of a `key: value` line, or `None` when there is none.
fn field_value<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    parts(line, key)
        .map(|(_, value, ..)| value)
        .filter(|value| !value.is_empty())
}

/// Splits a `key: value  # comment` line into everything up to the value, the
/// value, the space before the comment, and the comment.
fn parts<'a>(line: &'a str, key: &str) -> Option<(&'a str, &'a str, &'a str, &'a str)> {
    let indent = line.len() - line.trim_start().len();
    let after = indent + key.len() + 1;
    line.get(..after)
        .filter(|head| head.ends_with(':'))
        .zip(line.get(after..))
        .map(|(_, rest)| {
            let (rest, comment) = rest.split_at(comment_start(rest));
            let value = rest.trim();
            let value_at = rest.find(value).unwrap_or(rest.len());

            (
                &line[..after + value_at],
                value,
                &rest[value_at + value.len()..],
                comment,
            )
        })
}

/// Where a trailing comment starts, or the end of the text. A `#` only opens one
/// when it is outside quotes and follows a space, which is what keeps a value
/// like `#ff0000` intact.
fn comment_start(text: &str) -> usize {
    let mut quote = None;
    let mut previous = ' ';

    for (index, c) in text.char_indices() {
        match (quote, c) {
            (Some(open), c) if c == open => quote = None,
            (None, '"' | '\'') => quote = Some(c),
            (None, '#') if previous.is_whitespace() => return index,
            _ => (),
        }
        previous = c;
    }

    text.len()
}

/// Re-quotes a value the way the file already quoted it, or `None` if it was not
/// quoted or cannot be quoted that way without escapes this does not write.
fn quoted_like(old: &str, value: &str) -> Option<String> {
    let awkward = |c: char| c.is_control() || c == '\\';
    let wrapped = |quote: char| {
        old.len() >= 2 && old.starts_with(quote) && old.ends_with(quote) && !value.contains(awkward)
    };

    // A value serde_yaml itself quoted is left exactly as it rendered it.
    if value.starts_with(['"', '\'']) {
        return None;
    }

    match old {
        _ if wrapped('"') && !value.contains('"') => Some(format!("\"{value}\"")),
        _ if wrapped('\'') => Some(format!("'{}'", value.replace('\'', "''"))),
        _ => None,
    }
}

/// A top-level key the document does not have yet.
fn new_entry(key: &str, plan: &Plan) -> Result<Vec<String>> {
    Ok(match plan {
        Plan::Top(Shape::Absent) => Vec::new(),
        Plan::Top(Shape::Line { text, quoted, .. }) => {
            vec![format!("{key}: {}", appended(text, *quoted))]
        }
        Plan::Top(Shape::Block { body, .. }) => std::iter::once(format!("{key}:"))
            .chain(indented(body, INDENT))
            .collect(),
        Plan::Nested { value, .. } => std::iter::once(format!("{key}:"))
            .chain(indented(&serde_yaml::to_string(value)?, INDENT))
            .collect(),
    })
}
