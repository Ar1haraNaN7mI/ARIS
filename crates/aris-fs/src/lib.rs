//! Filesystem capability with PathGuard protecting the core kernel.

mod guard;
mod patch;
mod replacement;
mod search;

pub use guard::{PathDecision, PathGuard, PathGuardConfig};
pub use search::GrepOptions;

use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

pub struct FsService {
    guard: PathGuard,
}

impl FsService {
    pub fn new(guard: PathGuard) -> Self {
        Self { guard }
    }

    pub fn guard(&self) -> &PathGuard {
        &self.guard
    }

    pub fn read_text(&self, path: impl AsRef<Path>) -> Result<String> {
        let path = self.guard.resolve_path(path.as_ref());
        self.guard.check_read(&path)?;
        fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))
    }

    /// 1-based line offset; `limit` caps number of lines returned.
    pub fn read_range(
        &self,
        path: impl AsRef<Path>,
        offset: Option<usize>,
        limit: Option<usize>,
    ) -> Result<String> {
        let path = self.guard.resolve_path(path.as_ref());
        self.guard.check_read(&path)?;
        let file = fs::File::open(&path).with_context(|| format!("open {}", path.display()))?;
        let reader = BufReader::new(file);
        let start = offset.unwrap_or(1).max(1);
        let max_lines = limit.unwrap_or(400).max(1);
        let mut out = String::new();
        let mut taken = 0usize;
        let mut total = 0usize;
        for (idx, line) in reader.lines().enumerate() {
            total += 1;
            let line_no = idx + 1;
            if line_no < start {
                continue;
            }
            if taken >= max_lines {
                continue;
            }
            let line = line?;
            out.push_str(&format!("{line_no:>6}|{line}\n"));
            taken += 1;
        }
        if start > total && total > 0 {
            bail!(
                "offset {start} past end ({total} lines) of {}",
                path.display()
            );
        }
        if taken > 0 && start.saturating_add(taken - 1) < total {
            out.push_str(&format!(
                "… truncated; showing {taken} lines from {start}; file has {total} lines\n"
            ));
        }
        Ok(out)
    }

    pub fn write_text(&self, path: impl AsRef<Path>, content: &str) -> Result<()> {
        let path = self.guard.resolve_path(path.as_ref());
        self.guard.check_write(&path)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension(format!(
            "{}.tmp",
            path.extension().and_then(|s| s.to_str()).unwrap_or("aris")
        ));
        fs::write(&tmp, content).with_context(|| format!("write temp {}", tmp.display()))?;
        replace_file(&tmp, &path)?;
        Ok(())
    }

    pub fn edit_replace(
        &self,
        path: impl AsRef<Path>,
        old: &str,
        new: &str,
        replace_all: bool,
    ) -> Result<String> {
        let path = self.guard.resolve_path(path.as_ref());
        let original = self.read_text(&path)?;
        if old.is_empty() {
            bail!("old_string must not be empty");
        }
        let (updated, count) = replacement::replace_anchor(
            &path,
            &original,
            old,
            new,
            replacement::Mode::Edit { replace_all },
        )?;
        self.write_text(&path, &updated)?;
        Ok(format!(
            "updated {} ({} replacement{})",
            path.display(),
            if replace_all { count } else { 1 },
            if replace_all && count != 1 { "s" } else { "" }
        ))
    }

    /// Apply a simplified multi-hunk patch:
    /// ```text
    /// *** Update File: path
    /// <<<<<<< SEARCH
    /// old
    /// =======
    /// new
    /// >>>>>>> REPLACE
    /// ```
    /// Validate the patch and report the files/hunks that would change.
    ///
    /// This deliberately shares the parser and planner used by
    /// [`Self::apply_patch`], so a dry-run cannot silently accept a different
    /// patch dialect than a real apply.
    pub fn preview_patch(&self, patch: &str) -> Result<PatchPreview> {
        let plan = self.plan_patch(patch)?;
        let mut files: Vec<String> = plan
            .contents
            .keys()
            .map(|path| path.display().to_string())
            .collect();
        files.extend(plan.deletions.iter().map(|path| path.display().to_string()));
        files.sort();
        Ok(PatchPreview {
            files,
            hunks: plan.hunk_count,
            operations: plan.operations,
        })
    }

    pub fn apply_patch(&self, patch: &str) -> Result<String> {
        let plan = self.plan_patch(patch)?;
        for (path, content) in &plan.contents {
            self.write_text(path, content)?;
        }
        for path in &plan.deletions {
            self.guard.check_write(path)?;
            if path.exists() {
                fs::remove_file(path).with_context(|| format!("delete {}", path.display()))?;
            }
        }
        Ok(plan.operations.join("\n"))
    }

    fn plan_patch(&self, patch: &str) -> Result<PatchPlan> {
        let sections = patch::parse(patch)?;
        let mut contents = BTreeMap::<PathBuf, String>::new();
        let mut deletions = BTreeSet::<PathBuf>::new();
        let mut operations = Vec::new();

        for section in sections {
            let path = self.guard.resolve_path(&section.path);
            self.guard.check_write(&path)?;
            if section.kind == patch::Kind::Delete {
                if contents.contains_key(&path) {
                    bail!("cannot delete and update {} in one patch", path.display());
                }
                if !path.is_file() {
                    bail!("cannot delete missing file {}", path.display());
                }
                deletions.insert(path.clone());
                operations.push(format!("deleted {}", path.display()));
                continue;
            }
            if deletions.contains(&path) {
                bail!("cannot update deleted file {} in one patch", path.display());
            }
            let existing = contents.remove(&path);
            let current = if let Some(current) = existing {
                current
            } else if section.kind == patch::Kind::Add && !path.try_exists()? {
                String::new()
            } else {
                self.read_text(&path)?
            };
            if section.kind == patch::Kind::Add && !current.is_empty() {
                bail!(
                    "Add File would overwrite nonempty {}; use Update File with a unique anchor.",
                    path.display()
                );
            }
            let (updated, reports) = section.apply(&path, current)?;
            contents.insert(path, updated);
            operations.extend(reports);
        }

        Ok(PatchPlan {
            contents,
            deletions,
            hunk_count: operations.len(),
            operations,
        })
    }

    pub fn list_dir(&self, path: impl AsRef<Path>) -> Result<Vec<PathBuf>> {
        let path = self.guard.resolve_path(path.as_ref());
        self.guard.check_read(&path)?;
        let mut entries = Vec::new();
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            entries.push(entry.path());
        }
        entries.sort();
        Ok(entries)
    }

    pub fn glob(&self, root: &Path, pattern: &str, max: usize) -> Result<Vec<PathBuf>> {
        if max == 0 {
            return Ok(Vec::new());
        }
        let root = self.guard.resolve_path(root);
        let matcher = glob::Pattern::new(pattern).context("invalid glob pattern")?;
        let mut hits = Vec::new();
        self.guard.check_read(&root)?;
        for entry in WalkDir::new(&root).into_iter().filter_map(|e| e.ok()) {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            let rel = path.strip_prefix(&root).unwrap_or(path);
            let rel_s = rel.to_string_lossy().replace('\\', "/");
            if matcher.matches(&rel_s)
                || matcher.matches(path.file_name().and_then(|s| s.to_str()).unwrap_or(""))
            {
                hits.push(path.to_path_buf());
                if hits.len() >= max {
                    break;
                }
            }
        }
        Ok(hits)
    }
}

fn replace_file(temp: &Path, destination: &Path) -> Result<()> {
    if let Err(first_error) = fs::rename(temp, destination) {
        if cfg!(windows) && destination.is_file() {
            fs::remove_file(destination)
                .with_context(|| format!("remove old file {}", destination.display()))?;
            fs::rename(temp, destination).with_context(|| {
                format!(
                    "rename temp file to {} after replace",
                    destination.display()
                )
            })?;
        } else {
            let _ = fs::remove_file(temp);
            return Err(first_error)
                .with_context(|| format!("rename temp file to {}", destination.display()));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct PatchPreview {
    pub files: Vec<String>,
    pub hunks: usize,
    pub operations: Vec<String>,
}

struct PatchPlan {
    contents: BTreeMap<PathBuf, String>,
    deletions: BTreeSet<PathBuf>,
    operations: Vec<String>,
    hunk_count: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "aris-fs-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn service(root: &Path) -> FsService {
        FsService::new(
            PathGuard::new(PathGuardConfig {
                workspace_root: root.to_path_buf(),
                outer_home: root.join("outer"),
                workspace_outer: PathBuf::from(".aris"),
                deny_core_writes: true,
                deny_patterns: Vec::new(),
            })
            .expect("guard"),
        )
    }

    #[test]
    fn patch_preview_and_apply_support_delete_headers() {
        let root = temp_root("delete");
        std::fs::create_dir_all(&root).expect("root");
        let path = root.join("remove.txt");
        std::fs::write(&path, "remove me\n").expect("file");
        let fs = service(&root);
        let patch = format!("*** Delete File: {}\n", path.display());
        let preview = fs.preview_patch(&patch).expect("preview");
        assert_eq!(preview.hunks, 1);
        assert_eq!(preview.files, vec![path.display().to_string()]);
        fs.apply_patch(&patch).expect("apply");
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn truncated_or_invalid_patch_leaves_every_file_untouched() {
        let root = temp_root("patch-incomplete");
        std::fs::create_dir_all(&root).unwrap();
        let fs = service(&root);
        fs.write_text("victim.txt", "keep\n").unwrap();
        let first = "*** Add File: staged.txt\n<<<<<<< SEARCH\n=======\nstaged\n>>>>>>> REPLACE\n";
        for (body, expected) in [
            ("*** Update File: victim.txt\n<<<<<<< SEARCH\nkeep", "======="),
            ("*** Update File: victim.txt\n<<<<<<< SEARCH\nkeep\n=======\nreplaced", ">>>>>>> REPLACE"),
            ("*** Update File: victim.txt\n<<<<<<< SEARCH\nkeep\n*** Delete File: victim.txt", "======="),
            ("*** Update File: victim.txt\n<<<<<<< SEARCH\nkeep\n=======\nreplace\n*** Delete File: victim.txt", ">>>>>>> REPLACE"),
            ("*** Update File: victim.txt\n<<<<<<< SEARCH\nkeep\n>>>>>>> REPLACE", "======="),
            ("*** Update File: victim.txt\n<<<<<<< SEARCH\nkeep\n<<<<<<< SEARCH", "======="),
            ("*** Update File: victim.txt\n=======\nreplace\n>>>>>>> REPLACE", "<<<<<<< SEARCH"),
            ("*** Update File: victim.txt\n", "<<<<<<< SEARCH"),
            ("*** Add File: \n", "path"),
        ] {
            let patch = format!("{first}{body}");
            let error = fs.preview_patch(&patch).unwrap_err().to_string();
            assert!(error.contains(expected), "{error}");
            assert!(fs.apply_patch(&patch).is_err());
            assert_eq!(fs.read_text("victim.txt").unwrap(), "keep\n");
            assert!(!root.join("staged.txt").exists());
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn add_and_empty_search_preserve_nonempty_targets() {
        let root = temp_root("patch-existing");
        std::fs::create_dir_all(&root).unwrap();
        let fs = service(&root);
        fs.write_text("existing.txt", "keep").unwrap();
        for patch in [
            "*** Add File: existing.txt\n<<<<<<< SEARCH\n=======\nnew\n>>>>>>> REPLACE",
            "*** Update File: existing.txt\n<<<<<<< SEARCH\n=======\nnew\n>>>>>>> REPLACE",
            "*** Update File: existing.txt\n<<<<<<< SEARCH\n=======\n>>>>>>> REPLACE",
            "*** Begin Patch\n*** Add File: existing.txt\n+new\n*** End Patch",
        ] {
            assert!(fs.preview_patch(patch).is_err());
            assert!(fs.apply_patch(patch).is_err());
            assert_eq!(fs.read_text("existing.txt").unwrap(), "keep");
        }
        for kind in ["Update", "Add"] {
            fs.write_text("empty.txt", "").unwrap();
            let patch = format!(
                "*** {kind} File: empty.txt\n<<<<<<< SEARCH\n=======\nnew\n>>>>>>> REPLACE"
            );
            fs.apply_patch(&patch).unwrap();
            assert_eq!(fs.read_text("empty.txt").unwrap(), "new");
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn wrapped_patch_supports_multiple_operations_and_preserves_source_endings() {
        let root = temp_root("diff-multiple");
        std::fs::create_dir_all(&root).unwrap();
        let fs = service(&root);
        let before = "one\r\nkeep\nold\r\nmiddle\r\nlast";
        fs.write_text("source.txt", before).unwrap();
        fs.write_text("obsolete.txt", "gone").unwrap();
        let patch = "*** Begin Patch\n*** Update File: source.txt\n@@\n-one\n+first\n@@ keep\n-old\n+new\n+added\n middle\n-last\n+tail\n*** End of File\n*** Add File: nested/new.txt\n+created\n+\n*** Delete File: obsolete.txt\n*** End Patch";
        let preview = fs.preview_patch(patch).unwrap();
        assert_eq!(preview.hunks, 4);
        assert_eq!(preview.files.len(), 3);
        assert_eq!(fs.read_text("source.txt").unwrap(), before);
        assert!(root.join("obsolete.txt").exists());
        assert!(!root.join("nested").exists());
        fs.apply_patch(patch).unwrap();
        assert_eq!(
            fs.read_text("source.txt").unwrap(),
            "first\r\nkeep\nnew\r\nadded\r\nmiddle\r\ntail"
        );
        assert_eq!(fs.read_text("nested/new.txt").unwrap(), "created\n\n");
        assert!(!root.join("obsolete.txt").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn wrapped_patch_requires_complete_structure_and_unique_matching_context() {
        let root = temp_root("diff-incomplete");
        std::fs::create_dir_all(&root).unwrap();
        let fs = service(&root);
        let original = "first\nrepeat\nrepeat\n";
        fs.write_text("source.txt", original).unwrap();
        let first =
            "*** Begin Patch\n*** Delete File: source.txt\n*** Add File: staged.txt\n+new\n";
        for suffix in [
            "",
            "*** End Patc",
            "*** Add File: broken.txt\nunprefixed\n*** End Patch",
        ] {
            let input = format!("{first}{suffix}");
            assert!(fs.preview_patch(&input).is_err());
            assert!(fs.apply_patch(&input).is_err());
            assert_eq!(fs.read_text("source.txt").unwrap(), original);
            assert!(!root.join("staged.txt").exists());
        }
        for hunk in ["-repeat\n+x", "-missing\n+x", "@@", "repeat\n+x"] {
            let input = format!("*** Begin Patch\n*** Add File: staged.txt\n+new\n*** Update File: source.txt\n@@\n{hunk}\n*** End Patch");
            assert!(fs.apply_patch(&input).is_err());
            assert_eq!(fs.read_text("source.txt").unwrap(), original);
            assert!(!root.join("staged.txt").exists());
        }
        fs.apply_patch("*** Begin Patch\n*** Update File: source.txt\n@@\n-repeat\n+final\n*** End of File\n*** End Patch").unwrap();
        assert_eq!(
            fs.read_text("source.txt").unwrap(),
            "first\nrepeat\nfinal\n"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn patch_parser_accepts_a_utf8_bom_from_windows_editors() {
        let root = temp_root("bom");
        std::fs::create_dir_all(&root).expect("root");
        let fs = service(&root);
        let patch =
            "\u{feff}*** Add File: bom.txt\n<<<<<<< SEARCH\n=======\nencoded\n>>>>>>> REPLACE\n";
        fs.apply_patch(patch).expect("apply BOM patch");
        assert_eq!(
            std::fs::read_to_string(root.join("bom.txt")).expect("read"),
            "encoded"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn relative_write_is_checked_against_workspace_root() {
        let root = temp_root("relative");
        std::fs::create_dir_all(&root).expect("root");
        let fs = service(&root);
        fs.write_text(Path::new("safe.txt"), "ok").expect("write");
        assert_eq!(
            std::fs::read_to_string(root.join("safe.txt")).unwrap(),
            "ok"
        );
        let decision = fs.guard().decide_write(Path::new("crates/blocked.rs"));
        assert!(
            matches!(decision, PathDecision::Deny(_)),
            "decision={decision:?}"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn write_text_replaces_existing_file_on_all_platforms() {
        let root = temp_root("replace");
        std::fs::create_dir_all(&root).expect("root");
        let fs = service(&root);
        fs.write_text(Path::new("value.txt"), "one").expect("first");
        fs.write_text(Path::new("value.txt"), "two")
            .expect("replace");
        assert_eq!(
            std::fs::read_to_string(root.join("value.txt")).unwrap(),
            "two"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn read_range_only_reports_truncation_when_lines_are_omitted() {
        let root = temp_root("read-range");
        std::fs::create_dir_all(&root).expect("root");
        let fs = service(&root);
        fs.write_text(Path::new("lines.txt"), "one\ntwo\nthree\n")
            .expect("write");

        let complete = fs
            .read_range(Path::new("lines.txt"), None, Some(10))
            .expect("complete range");
        assert!(!complete.contains("truncated"));
        let partial = fs
            .read_range(Path::new("lines.txt"), None, Some(2))
            .expect("partial range");
        assert!(partial.contains("truncated"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn glob_with_zero_limit_returns_no_matches() {
        let root = temp_root("glob-zero");
        std::fs::create_dir_all(&root).expect("root");
        let fs = service(&root);
        fs.write_text(Path::new("file.txt"), "content")
            .expect("write");
        assert!(fs.glob(Path::new("."), "*", 0).expect("glob").is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn copied_multiline_anchor_preserves_local_endings_for_edits_and_patches() {
        let root = temp_root("crlf-patch");
        fs::create_dir_all(&root).unwrap();
        let service = service(&root);
        let original = "第一行\r\n\r\nalpha\r\nbeta\ngamma\r\n末尾";
        let expected = "第一行\r\n\r\n一\r\n二\n三\n四\r\n末尾";
        for file in ["edit.txt", "patch.txt"] {
            service.write_text(file, original).unwrap();
        }
        let old = "\nalpha\nbeta\ngamma";
        let new = "\n一\n二\n三\n四";
        service.edit_replace("edit.txt", old, new, false).unwrap();
        assert_eq!(service.read_text("edit.txt").unwrap(), expected);
        let patch = format!(
            "*** Update File: patch.txt\n<<<<<<< SEARCH\n{old}\n=======\n{new}\n>>>>>>> REPLACE\n"
        );
        assert_eq!(service.preview_patch(&patch).unwrap().hunks, 1);
        assert_eq!(service.read_text("patch.txt").unwrap(), original);
        service.apply_patch(&patch).unwrap();
        assert_eq!(service.read_text("patch.txt").unwrap(), expected);
        // An added line in the unterminated last line inherits the prior CRLF.
        service
            .edit_replace("edit.txt", "末尾", "末尾\ntail", false)
            .unwrap();
        assert_eq!(
            service.read_text("edit.txt").unwrap(),
            format!("{expected}\r\ntail")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ambiguous_logical_anchors_remain_ambiguous_and_explicit_crlf_is_exact() {
        let root = temp_root("mixed-anchors");
        fs::create_dir_all(&root).unwrap();
        let service = service(&root);
        let original = "头\r\na\r\nb\r\n中\na\nb\n尾";
        service.write_text("source.txt", original).unwrap();
        let failure = service
            .edit_replace("source.txt", "a\nb", "一\n二", false)
            .unwrap_err();
        assert!(failure.to_string().contains("found 2 times"));
        let patch =
            "*** Update File: source.txt\n<<<<<<< SEARCH\na\nb\n=======\n一\n二\n>>>>>>> REPLACE\n";
        assert!(service
            .preview_patch(patch)
            .unwrap_err()
            .to_string()
            .contains("found 2 times"));
        assert!(service.apply_patch(patch).is_err());
        assert_eq!(service.read_text("source.txt").unwrap(), original);
        service
            .edit_replace("source.txt", "a\nb", "一\n二", true)
            .unwrap();
        assert_eq!(
            service.read_text("source.txt").unwrap(),
            "头\r\n一\r\n二\r\n中\n一\n二\n尾"
        );
        // Explicit CRLF in the anchor addresses only that byte spelling,
        // and an explicitly mixed replacement is written as supplied.
        service
            .edit_replace("source.txt", "一\r\n二", "first\nsecond", false)
            .unwrap();
        service
            .edit_replace("source.txt", "一\n二", "A\r\nB\nC", false)
            .unwrap();
        assert_eq!(
            service.read_text("source.txt").unwrap(),
            "头\r\nfirst\nsecond\r\n中\nA\r\nB\nC\n尾"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn grep_finds_source_names_that_contain_generated_directory_names() {
        let root = temp_root("source-search");
        for name in [
            "target/generated.txt",
            "web/node_modules/pkg/generated.txt",
            ".git/config",
            ".aris/review.json",
            "src/retarget.rs",
            "node_modules_notes/readme.md",
            ".github/workflows/ci.yml",
        ] {
            let path = root.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "SOURCE_MARK\n").unwrap();
        }
        let service = service(&root);
        let hits = service
            .grep(Path::new("."), "SOURCE_MARK", None, 80, false)
            .unwrap();
        assert_eq!(hits,".github/workflows/ci.yml:1:SOURCE_MARK\nnode_modules_notes/readme.md:1:SOURCE_MARK\nsrc/retarget.rs:1:SOURCE_MARK\n");
        assert_eq!(
            service
                .grep(Path::new("target"), "SOURCE_MARK", None, 80, false)
                .unwrap(),
            "generated.txt:1:SOURCE_MARK\n"
        );
        assert!(service
            .grep(Path::new("missing"), ".", None, 80, false)
            .is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn grep_single_file_literal_overlapping_context_and_pagination() {
        let root = temp_root("grep-page");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("source.rs"),
            "开头\r\nAPI[a]\r\n中间\r\napi[a]\r\n尾部\r\napia\r\n",
        )
        .unwrap();
        let service = service(&root);
        let options = GrepOptions {
            glob: Some("*.rs"),
            literal: true,
            case_insensitive: true,
            context_lines: 1,
            max_hits: 1,
            offset: 0,
        };
        let first = service
            .grep_with_options(Path::new("source.rs"), "api[a]", options)
            .unwrap();
        assert_eq!(first,"source.rs-1-开头\nsource.rs:2:API[a]\nsource.rs-3-中间\n… hit limit reached; next offset=1\n");
        let next = service
            .grep_with_options(
                Path::new("source.rs"),
                "api[a]",
                GrepOptions {
                    offset: 1,
                    ..options
                },
            )
            .unwrap();
        assert_eq!(
            next,
            "source.rs-3-中间\nsource.rs:4:api[a]\nsource.rs-5-尾部\n"
        );
        let both = service
            .grep_with_options(
                Path::new("."),
                "api[a]",
                GrepOptions {
                    max_hits: 2,
                    ..options
                },
            )
            .unwrap();
        assert_eq!(both,"source.rs-1-开头\nsource.rs:2:API[a]\nsource.rs-3-中间\nsource.rs:4:api[a]\nsource.rs-5-尾部\n");
        assert_eq!(
            service
                .grep(Path::new("source.rs"), "api[a]", None, 1, true)
                .unwrap(),
            "source.rs:6:apia\n"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn write_through_an_outside_symlink_is_denied() {
        let root = temp_root("symlink-root");
        let outside = temp_root("symlink-outside");
        std::fs::create_dir_all(&root).expect("root");
        std::fs::create_dir_all(&outside).expect("outside");
        let link = root.join("linked");
        let symlink_result = create_directory_symlink(&outside, &link);
        if symlink_result.is_err() {
            // Windows may run without the privilege required to create a
            // directory symlink; retain the test on platforms where it is
            // available instead of turning an environment limitation into a
            // false failure.
            let _ = std::fs::remove_dir_all(&root);
            let _ = std::fs::remove_dir_all(&outside);
            return;
        }
        let fs = service(&root);
        let decision = fs.guard().decide_write(Path::new("linked/new.txt"));
        assert!(
            matches!(decision, PathDecision::Deny(_)),
            "decision={decision:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[cfg(unix)]
    fn create_directory_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }

    #[cfg(windows)]
    fn create_directory_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
        std::os::windows::fs::symlink_dir(target, link)
    }
}
