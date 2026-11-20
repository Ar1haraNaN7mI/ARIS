//! Both patch dialects are parsed in full before the workspace planner runs.
mod unified;

use crate::replacement;
use anyhow::{anyhow, bail, ensure, Result};
use std::path::{Path, PathBuf};

const EXAMPLE: &str = "Use *** Begin Patch, *** Update File: path, @@, -old and +new lines, then *** End Patch. For SEARCH/REPLACE, use *** Update File: path, <<<<<<< SEARCH, old text, =======, new text, >>>>>>> REPLACE on separate lines.";

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Update,
    Add,
    Delete,
}

pub(super) struct Section<'a> {
    pub path: PathBuf,
    pub kind: Kind,
    edits: Edits<'a>,
}

enum Edits<'a> {
    Anchors(Vec<(String, String)>),
    Lines(Vec<unified::Hunk<'a>>),
    Delete,
}

impl Section<'_> {
    pub fn apply(&self, path: &Path, mut current: String) -> Result<(String, Vec<String>)> {
        let mut operations = Vec::new();
        match &self.edits {
            Edits::Anchors(anchors) => {
                for (old, new) in anchors {
                    if old.is_empty() {
                        ensure!(
                            current.is_empty(),
                            "Empty SEARCH requires an empty file; copy a unique anchor from {}.",
                            path.display()
                        );
                        current.clone_from(new);
                        operations.push(format!("wrote {}", path.display()));
                    } else {
                        (current, _) = replacement::replace_anchor(
                            path,
                            &current,
                            old,
                            new,
                            replacement::Mode::Patch,
                        )?;
                        operations.push(format!("updated {} (1 replacement)", path.display()));
                    }
                }
            }
            Edits::Lines(hunks) => {
                current = unified::apply(&current, hunks)
                    .map_err(|error| anyhow!("update {} failed: {error}", path.display()))?;
                operations.extend(hunks.iter().map(|_| format!("updated {}", path.display())));
            }
            Edits::Delete => unreachable!("planner handles deletes"),
        }
        Ok((current, operations))
    }
}

fn header(line: &str) -> Option<(Kind, &str)> {
    [
        ("*** Update File:", Kind::Update),
        ("*** Add File:", Kind::Add),
        ("*** Delete File:", Kind::Delete),
    ]
    .into_iter()
    .find_map(|(prefix, kind)| line.strip_prefix(prefix).map(|path| (kind, path.trim())))
}

pub(super) fn parse(patch: &str) -> Result<Vec<Section<'_>>> {
    let lines: Vec<_> = patch.trim_start_matches('\u{feff}').lines().collect();
    let mut lines = lines.as_slice();
    while lines.first().is_some_and(|l| l.trim().is_empty()) {
        lines = &lines[1..];
    }
    let wrapped = lines.first() == Some(&"*** Begin Patch");
    if wrapped {
        while lines.last().is_some_and(|l| l.trim().is_empty()) {
            lines = &lines[..lines.len() - 1];
        }
        ensure!(
            lines.last() == Some(&"*** End Patch"),
            "Incomplete patch: expected *** End Patch. {EXAMPLE}"
        );
        lines = &lines[1..lines.len() - 1];
    }
    let mut sections = Vec::new();
    while let Some((&line, remaining)) = lines.split_first() {
        if line.trim().is_empty() {
            lines = remaining;
            continue;
        }
        let Some((kind, path)) = header(line) else {
            bail!("Expected a file header, found {line:?}. {EXAMPLE}");
        };
        ensure!(
            !path.is_empty(),
            "Patch header requires a file path. {EXAMPLE}"
        );
        let next = remaining
            .iter()
            .position(|l| header(l).is_some())
            .unwrap_or(remaining.len());
        let (body, rest) = remaining.split_at(next);
        lines = rest;
        let first = body
            .iter()
            .position(|l| !l.trim().is_empty())
            .unwrap_or(body.len());
        let edits = if kind == Kind::Delete {
            ensure!(
                first == body.len(),
                "Delete File takes a header only: {path}. {EXAMPLE}"
            );
            Edits::Delete
        } else if body
            .get(first)
            .is_some_and(|l| l.trim() == "<<<<<<< SEARCH")
        {
            Edits::Anchors(anchors(&body[first..])?)
        } else if wrapped && kind == Kind::Add {
            let mut content = String::new();
            for line in body {
                let Some(added) = line.strip_prefix('+') else {
                    bail!("Expected +line under Add File: {path}. {EXAMPLE}");
                };
                content.push_str(added);
                content.push('\n');
            }
            Edits::Anchors(vec![(String::new(), content)])
        } else if wrapped {
            Edits::Lines(unified::parse(body)?)
        } else {
            bail!("Expected <<<<<<< SEARCH for {path}. {EXAMPLE}");
        };
        sections.push(Section {
            path: path.into(),
            kind,
            edits,
        });
    }
    ensure!(
        !sections.is_empty(),
        "Patch contains no file operations. {EXAMPLE}"
    );
    Ok(sections)
}

fn anchors(mut lines: &[&str]) -> Result<Vec<(String, String)>> {
    let mut edits = Vec::new();
    while let Some((&first, rest)) = lines.split_first() {
        lines = rest;
        if first.trim().is_empty() {
            continue;
        }
        ensure!(
            first.trim() == "<<<<<<< SEARCH",
            "Expected <<<<<<< SEARCH. {EXAMPLE}"
        );
        let mut section = |expected| -> Result<String> {
            let delimiter = lines.iter().position(|line| {
                matches!(
                    line.trim(),
                    "<<<<<<< SEARCH" | "=======" | ">>>>>>> REPLACE"
                )
            });
            let Some(i) = delimiter else {
                bail!("Incomplete hunk: expected {expected}. {EXAMPLE}");
            };
            ensure!(
                lines[i].trim() == expected,
                "Invalid hunk: expected {expected}, found {}. {EXAMPLE}",
                lines[i].trim()
            );
            let text = lines[..i].join("\n");
            lines = &lines[i + 1..];
            Ok(text)
        };
        edits.push((section("=======")?, section(">>>>>>> REPLACE")?));
    }
    Ok(edits)
}
