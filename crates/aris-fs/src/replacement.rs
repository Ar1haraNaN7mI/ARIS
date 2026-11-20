use anyhow::{bail, Result};
use std::borrow::Cow;
use std::path::Path;

pub(crate) enum Mode {
    Edit { replace_all: bool },
    Patch,
}

/// read_file presents logical lines. An LF anchor copied from that view
/// must identify the same source range on LF, CRLF and mixed-ending files.
/// Callers that supply carriage returns explicitly keep byte-exact control.
pub(crate) fn replace_anchor(
    path: &Path,
    source: &str,
    old: &str,
    new: &str,
    mode: Mode,
) -> Result<(String, usize)> {
    debug_assert!(!old.is_empty());
    let explicit_anchor = old.contains('\r');
    let normalize = !explicit_anchor && old.contains('\n') && source.contains("\r\n");
    let (searchable, removed_cr) = if normalize {
        // Each entry is the logical position of an LF whose raw spelling starts
        // with CR. At that exact position a match must include the original CR.
        let positions: Vec<_> = source
            .match_indices("\r\n")
            .enumerate()
            .map(|(removed, (raw, _))| raw - removed)
            .collect();
        (Cow::Owned(source.replace("\r\n", "\n")), positions)
    } else {
        (Cow::Borrowed(source), Vec::new())
    };
    let count = searchable.matches(old).count();
    match (count, mode) {
        (0, _) => bail!("old_string not found in {}", path.display()),
        (2.., Mode::Edit { replace_all: false }) => {
            bail!("old_string found {count} times; set replace_all=true or make it unique")
        }
        (2.., Mode::Patch) => bail!(
            "old_string found {count} times; make it unique before applying {}",
            path.display()
        ),
        _ => {}
    }
    let raw_offset = |logical: usize| logical + removed_cr.partition_point(|&i| i < logical);
    let local_endings =
        !explicit_anchor && !new.contains('\r') && new.contains('\n') && source.contains("\r\n");
    let mut output = String::with_capacity(source.len());
    let mut previous = 0;
    for (logical, _) in searchable.match_indices(old) {
        let start = raw_offset(logical);
        let end = raw_offset(logical + old.len());
        output.push_str(&source[previous..start]);
        if local_endings {
            append_using_local_endings(&mut output, source, start, end, new);
        } else {
            output.push_str(new);
        }
        previous = end;
    }
    output.push_str(&source[previous..]);
    Ok((output, count))
}

fn append_using_local_endings(
    out: &mut String,
    source: &str,
    start: usize,
    end: usize,
    replacement: &str,
) {
    let ending_at = |lf: usize| {
        if lf > 0 && source.as_bytes()[lf - 1] == b'\r' {
            "\r\n"
        } else {
            "\n"
        }
    };
    let mut existing = source[start..end]
        .match_indices('\n')
        .map(|(i, _)| ending_at(start + i));
    // Newlines inside a mixed-ending anchor keep their individual spellings.
    // Inserted lines inherit the last matched ending, or the enclosing line's
    // ending for a single-line anchor (the previous line for unterminated EOF).
    let mut previous = None;
    let mut lines = replacement.split('\n');
    out.push_str(lines.next().unwrap_or_default());
    for line in lines {
        let ending = existing.next().or(previous).unwrap_or_else(|| {
            source[end..]
                .find('\n')
                .map(|i| end + i)
                .or_else(|| source[..start].rfind('\n'))
                .map(ending_at)
                .unwrap_or("\n")
        });
        out.push_str(ending);
        out.push_str(line);
        previous = Some(ending);
    }
}
