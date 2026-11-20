use crate::FsService;
use anyhow::{Context, Result};
use regex::{Regex, RegexBuilder};
use std::collections::VecDeque;
use std::fmt::Write;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use walkdir::{DirEntry, WalkDir};

#[derive(Clone, Copy)]
pub struct GrepOptions<'a> {
    pub glob: Option<&'a str>,
    pub case_insensitive: bool,
    pub literal: bool,
    pub context_lines: usize,
    /// Number of matching lines to skip across all files.
    pub offset: usize,
    pub max_hits: usize,
}

impl Default for GrepOptions<'_> {
    fn default() -> Self {
        Self {
            glob: None,
            case_insensitive: false,
            literal: false,
            context_lines: 0,
            offset: 0,
            max_hits: 80,
        }
    }
}

impl FsService {
    pub fn grep(
        &self,
        root: &Path,
        pattern: &str,
        glob: Option<&str>,
        max_hits: usize,
        case_insensitive: bool,
    ) -> Result<String> {
        self.grep_with_options(
            root,
            pattern,
            GrepOptions {
                glob,
                max_hits,
                case_insensitive,
                ..Default::default()
            },
        )
    }

    pub fn grep_with_options(
        &self,
        root: &Path,
        pattern: &str,
        options: GrepOptions<'_>,
    ) -> Result<String> {
        let expression = if options.literal {
            regex::escape(pattern)
        } else {
            pattern.into()
        };
        let re = RegexBuilder::new(&expression)
            .case_insensitive(options.case_insensitive)
            .build()?;
        let glob = options
            .glob
            .map(glob::Pattern::new)
            .transpose()
            .context("invalid glob")?;
        let root = self.guard().resolve_path(root);
        self.guard().check_read(&root)?;
        let mut page = MatchPage::default();
        if options.max_hits == 0 {
            return Ok("(zero matches requested)\n".into());
        }
        let mut unreadable = 0;
        let entries = WalkDir::new(&root)
            .sort_by_file_name()
            .into_iter()
            .filter_entry(searchable_entry);
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) if error.depth() == 0 => return Err(error.into()),
                Err(_) => {
                    unreadable += 1;
                    continue;
                }
            };
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            let relative = if entry.depth() == 0 {
                Path::new(entry.file_name())
            } else {
                path.strip_prefix(&root)?
            };
            let name = relative.to_string_lossy().replace('\\', "/");
            if glob.as_ref().is_some_and(|glob| !glob.matches(&name)) || binary_extension(path) {
                continue;
            }
            let scan = File::open(path)
                .and_then(|file| page.scan(BufReader::new(file), &name, &re, options));
            if let Err(error) = scan {
                if entry.depth() == 0 {
                    return Err(error).with_context(|| format!("read {}", path.display()));
                }
                unreadable += 1;
            }
            if page.more {
                break;
            }
        }
        if page.emitted == 0 {
            page.text.push_str("(no matches)\n");
        }
        if page.more {
            writeln!(
                page.text,
                "… hit limit reached; next offset={}",
                options.offset.saturating_add(page.emitted)
            )?;
        }
        if unreadable != 0 {
            writeln!(page.text, "… skipped {unreadable} unreadable entries")?;
        }
        Ok(page.text)
    }
}

// Prune entire directories, while allowing an explicitly selected root.
// Substrings such as "retarget" and ".github" are ordinary source names.
fn searchable_entry(entry: &DirEntry) -> bool {
    if entry.depth() == 0 {
        return true;
    }
    let name = entry.file_name().to_str();
    name != Some(".git")
        && !(entry.file_type().is_dir()
            && matches!(name, Some("target" | "node_modules" | ".aris")))
}

fn binary_extension(path: &Path) -> bool {
    path.extension().and_then(|s| s.to_str()).is_some_and(|s| {
        matches!(
            s.to_ascii_lowercase().as_str(),
            "exe" | "dll" | "so" | "png" | "jpg" | "jpeg" | "gif" | "pdf" | "zip" | "wasm"
        )
    })
}

#[derive(Default)]
struct MatchPage {
    skipped: usize,
    emitted: usize,
    more: bool,
    text: String,
}

impl MatchPage {
    fn include_match(&mut self, options: GrepOptions<'_>) -> bool {
        if self.skipped < options.offset {
            self.skipped += 1;
            return false;
        }
        if self.emitted == options.max_hits {
            self.more = true;
            return false;
        }
        self.emitted += 1;
        true
    }

    fn scan(
        &mut self,
        mut reader: impl BufRead,
        path: &str,
        re: &Regex,
        options: GrepOptions<'_>,
    ) -> std::io::Result<()> {
        if reader.fill_buf()?.contains(&0) {
            return Ok(());
        }
        let mut before = VecDeque::new();
        let mut through = 0;
        for (i, line) in reader.lines().enumerate() {
            let n = i + 1;
            if self.more && n > through {
                break;
            }
            let line = line?;
            if re.is_match(&line) && self.include_match(options) {
                for (number, text) in before.drain(..) {
                    writeln!(self.text, "{path}-{number}-{text}").unwrap();
                }
                writeln!(self.text, "{path}:{n}:{line}").unwrap();
                through = n.saturating_add(options.context_lines);
            } else if n <= through {
                writeln!(self.text, "{path}-{n}-{line}").unwrap();
            } else if options.context_lines > 0 {
                if before.len() == options.context_lines {
                    before.pop_front();
                }
                before.push_back((n, line));
            }
        }
        Ok(())
    }
}
