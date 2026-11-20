use super::EXAMPLE;
use anyhow::{bail, ensure, Result};

pub(super) struct Hunk<'a> {
    heading: Option<&'a str>,
    rows: Vec<Row<'a>>,
    eof: bool,
}

enum Row<'a> {
    Keep(&'a str),
    Remove(&'a str),
    Add(&'a str),
}

pub(super) fn parse<'a>(mut lines: &[&'a str]) -> Result<Vec<Hunk<'a>>> {
    let mut hunks = Vec::new();
    while let Some(&first) = lines.first() {
        let heading = if first == "@@" || first.starts_with("@@ ") {
            lines = &lines[1..];
            first.strip_prefix("@@ ").filter(|s| !s.is_empty())
        } else {
            ensure!(
                hunks.is_empty(),
                "Expected @@ before the next hunk. {EXAMPLE}"
            );
            None
        };
        let mut rows = Vec::new();
        let mut eof = false;
        let mut changed = false;
        while let Some((&line, rest)) = lines.split_first() {
            if line == "@@" || line.starts_with("@@ ") {
                break;
            }
            lines = rest;
            if line == "*** End of File" {
                eof = true;
                ensure!(
                    lines.is_empty(),
                    "*** End of File must close the last hunk. {EXAMPLE}"
                );
                break;
            }
            if let Some(s) = line.strip_prefix('+') {
                rows.push(Row::Add(s));
                changed = true;
            } else if let Some(s) = line.strip_prefix('-') {
                rows.push(Row::Remove(s));
                changed = true;
            } else if let Some(s) = line.strip_prefix(' ') {
                rows.push(Row::Keep(s));
            } else if line.is_empty() {
                rows.push(Row::Keep(""));
            } else {
                bail!("Unrecognized diff line {line:?}; prefix context with a space, additions with +, deletions with -. {EXAMPLE}");
            }
        }
        ensure!(changed, "Update hunk contains no changed lines. {EXAMPLE}");
        hunks.push(Hunk { heading, rows, eof });
    }
    ensure!(!hunks.is_empty(), "Update File requires a hunk. {EXAMPLE}");
    Ok(hunks)
}

#[derive(Clone, Copy)]
struct Line<'a> {
    text: &'a str,
    ending: &'a str,
}

impl<'a> Line<'a> {
    fn split(raw: &'a str) -> Self {
        for ending in ["\r\n", "\n"] {
            if let Some(text) = raw.strip_suffix(ending) {
                return Self { text, ending };
            }
        }
        Self {
            text: raw,
            ending: "",
        }
    }
}

fn anchor(source: &[Line<'_>], cursor: usize, hunk: &Hunk<'_>) -> Result<usize> {
    let mut from = cursor;
    if let Some(heading) = hunk.heading {
        let index = source[from..].iter().position(|line| line.text == heading);
        let Some(index) = index else {
            bail!("Could not find @@ context {heading:?}");
        };
        from += index + 1;
    }
    let old: Vec<_> = hunk
        .rows
        .iter()
        .filter_map(|r| match *r {
            Row::Keep(s) | Row::Remove(s) => Some(s),
            Row::Add(_) => None,
        })
        .collect();
    if old.is_empty() {
        return Ok(source.len());
    }
    let mut matches = source[from..]
        .windows(old.len())
        .enumerate()
        .filter(|(i, window)| {
            (!hunk.eof || from + i + old.len() == source.len())
                && window
                    .iter()
                    .zip(&old)
                    .all(|(line, &expected)| line.text == expected)
        });
    let Some((offset, _)) = matches.next() else {
        bail!(
            "Could not find the hunk's exact context starting with {:?}",
            old[0]
        );
    };
    ensure!(
        matches.next().is_none(),
        "Multiple matching hunks starting with {:?}; include more context.",
        old[0]
    );
    Ok(from + offset)
}

pub(super) fn apply(source: &str, hunks: &[Hunk<'_>]) -> Result<String> {
    let lines: Vec<_> = source.split_inclusive('\n').map(Line::split).collect();
    let fallback = lines
        .iter()
        .rev()
        .find(|l| !l.ending.is_empty())
        .map_or("\n", |l| l.ending);
    let mut output = Vec::new();
    let mut cursor = 0;
    for hunk in hunks {
        let start = anchor(&lines, cursor, hunk)?;
        output.extend_from_slice(&lines[cursor..start]);
        cursor = start;
        // Added lines take their line ending from the corresponding removed
        // row, or from the local context when this hunk only inserts lines.
        let mut removed = Vec::new();
        let mut removed_index = 0;
        let mut local = lines
            .get(start)
            .filter(|l| !l.ending.is_empty())
            .map_or(fallback, |l| l.ending);
        for row in &hunk.rows {
            match *row {
                Row::Keep(_) => {
                    let line = lines[cursor];
                    output.push(line);
                    if !line.ending.is_empty() {
                        local = line.ending;
                    }
                    cursor += 1;
                    removed.clear();
                    removed_index = 0;
                }
                Row::Remove(_) => {
                    let ending = lines[cursor].ending;
                    if !ending.is_empty() {
                        local = ending;
                    }
                    removed.push(local);
                    cursor += 1;
                }
                Row::Add(text) => {
                    let ending = removed.get(removed_index).copied().unwrap_or(local);
                    removed_index += 1;
                    output.push(Line { text, ending });
                }
            }
        }
    }
    output.extend_from_slice(&lines[cursor..]);
    let mut text = String::with_capacity(source.len());
    for (i, line) in output.iter().enumerate() {
        text.push_str(line.text);
        let last = i + 1 == output.len();
        if !last || source.is_empty() || source.ends_with('\n') {
            text.push_str(if line.ending.is_empty() {
                fallback
            } else {
                line.ending
            });
        }
    }
    Ok(text)
}
