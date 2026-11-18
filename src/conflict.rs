//! Proves whether a pending edit really conflicts with the file's current
//! holder.
//!
//! The guard's file-intent check can see that another live session holds a
//! file, but "held" is not the same as "would conflict". A peer reads the
//! holder's version and the caller's post-edit version, reconstructs the merge
//! base from git, and asks `git merge-file` — the same three-way merge git
//! itself uses — whether the two changes overlap. Only a proven conflict
//! denies; a proven clean merge may be allowed, and anything that cannot be
//! proven stays denied so a broken check can never make the guard weaker than
//! the unconditional deny it replaces.
//!
//! # Fail-safe
//!
//! Every error path returns [`ConflictVerdict::Unprovable`], which callers must
//! treat like today's unconditional deny. "Could not find out" is a real
//! outcome, not a synonym for either "clean" or "conflict", which is why the
//! result is a three-way enum rather than an `Option<bool>`.

use crate::intent::IntentLocation;
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};

/// The edit a caller is about to apply to the file.
// The adapter constructs these next; until then nothing builds one outside tests.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub enum PendingEdit<'a> {
    /// A targeted replacement: `old` becomes `new`, first occurrence only.
    Replace { old: &'a str, new: &'a str },
    /// A whole-file write: `content` is the entire post-edit file.
    Write { content: &'a str },
}

/// Whether the pending edit can be merged with the holder's version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictVerdict {
    /// The three-way merge is clean; permitting the edit cannot produce a conflict.
    Clean,
    /// The merge has real conflicts.
    Conflict,
    /// The check could not be completed; callers must deny (today's behaviour).
    Unprovable,
}

/// Decides whether `edit`, applied in `caller_cwd`, conflicts with the version
/// of the file the holder is editing.
///
/// The steps:
///
/// 1. Read the holder's file. Any error (missing, unreadable, not UTF-8) is
///    [`ConflictVerdict::Unprovable`].
/// 2. Read the caller's current file; a missing file reads as empty.
/// 3. Apply the edit in memory. A `Replace` whose `old` text is absent means
///    the payload and the file disagree, which is
///    [`ConflictVerdict::Unprovable`].
/// 4. Reconstruct the merge base: both HEADs, their merge base, and the path's
///    blob at that commit. A path absent from the base tree is a normal case,
///    not an error.
/// 5. Without a base blob (a file new on both sides, or unrelated histories),
///    identical versions are clean and anything else is a conflict.
/// 6. With a base, run `git merge-file` over the three versions. Exit code 0
///    is clean, a positive exit code is a conflict, and a negative code or a
///    spawn failure is unprovable.
// The adapter wires this in next; until then the binary has no caller for it.
#[allow(dead_code)]
pub fn check(caller_cwd: &Path, holder: &IntentLocation, edit: PendingEdit<'_>) -> ConflictVerdict {
    let Ok(holder_version) = fs::read_to_string(&holder.file) else {
        return ConflictVerdict::Unprovable;
    };
    let caller_current = match fs::read_to_string(caller_cwd.join(&holder.rel)) {
        Ok(current) => current,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(_) => return ConflictVerdict::Unprovable,
    };
    let caller_version = match edit {
        PendingEdit::Write { content } => content.to_string(),
        PendingEdit::Replace { old, new } => {
            // The adapter's payload and the caller's file disagree; applying
            // the edit would prove nothing about what the caller will write.
            if !caller_current.contains(old) {
                return ConflictVerdict::Unprovable;
            }
            caller_current.replacen(old, new, 1)
        }
    };

    let Some(base_version) = base_version(caller_cwd, holder) else {
        // No common blob to merge against: identical versions cannot conflict,
        // and anything else is a conflict we cannot rule out.
        return if holder_version == caller_version {
            ConflictVerdict::Clean
        } else {
            ConflictVerdict::Conflict
        };
    };

    merge_verdict(caller_cwd, &holder_version, &base_version, &caller_version)
}

/// Reconstructs the file's content at the merge base of the caller's and the
/// holder's HEADs.
///
/// `None` covers both "the path does not exist at the merge base" (a file new
/// on both sides) and "no usable base could be computed" (unrelated histories,
/// no commits, git missing). The caller treats both the same way: compare the
/// two full versions.
fn base_version(caller_cwd: &Path, holder: &IntentLocation) -> Option<String> {
    let caller_head = git_line(caller_cwd, &["rev-parse", "HEAD"])?;
    let holder_head = git_line(&holder.worktree, &["rev-parse", "HEAD"])?;
    let merge_base = git_line(caller_cwd, &["merge-base", &caller_head, &holder_head])?;
    let spec = format!("{merge_base}:{}", forward_slashes(&holder.rel));
    let blob = git_bytes(caller_cwd, &["show", &spec])?;
    Some(String::from_utf8_lossy(&blob).into_owned())
}

/// Runs the three-way merge and maps git's exit code to a verdict.
///
/// `git merge-file` documents 0 for a clean merge and a positive count of
/// conflicts otherwise; a negative return (reported by the shell as 255) or a
/// spawn failure means the check itself did not run.
fn merge_verdict(caller_cwd: &Path, holder: &str, base: &str, caller: &str) -> ConflictVerdict {
    let Some(files) = MergeFiles::new([holder, base, caller]) else {
        return ConflictVerdict::Unprovable;
    };
    let [holder_path, base_path, caller_path] = &files.paths;
    let args = [
        OsStr::new("merge-file"),
        OsStr::new("-p"),
        holder_path.as_os_str(),
        base_path.as_os_str(),
        caller_path.as_os_str(),
    ];
    let Some(output) = git_output(caller_cwd, &args) else {
        return ConflictVerdict::Unprovable;
    };
    match output.status.code() {
        Some(0) => ConflictVerdict::Clean,
        Some(code) if code > 0 => ConflictVerdict::Conflict,
        _ => ConflictVerdict::Unprovable,
    }
}

/// Three uniquely named files holding the versions passed to `git merge-file`.
/// Dropping the value removes all three on every return path, including a
/// failed write halfway through.
struct MergeFiles {
    paths: [PathBuf; 3],
}

/// Distinguishes temp files created by concurrent checks in one process.
static NEXT_TEMP_ID: AtomicU32 = AtomicU32::new(0);

impl MergeFiles {
    fn new(versions: [&str; 3]) -> Option<Self> {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir();
        let files = Self {
            paths: std::array::from_fn(|index| {
                dir.join(format!(
                    "portzilla-merge-{}-{id}-{index}",
                    std::process::id()
                ))
            }),
        };
        for (path, version) in files.paths.iter().zip(versions) {
            // `create_new` refuses to follow a pre-existing file (including a
            // stale one from a crashed process with the same pid), so a hostile
            // or unlucky temp directory cannot redirect the write.
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .ok()?;
            file.write_all(version.as_bytes()).ok()?;
        }
        Some(files)
    }
}

impl Drop for MergeFiles {
    fn drop(&mut self) {
        for path in &self.paths {
            let _ = fs::remove_file(path);
        }
    }
}

/// Runs `git -C cwd <args>` and returns its output when git itself could be
/// spawned. A non-zero exit is deliberately not swallowed: `merge-file` uses
/// the exit code as its answer.
///
/// This is the only place the module spawns a process, so the test-only fork
/// cutoff guard cannot be forgotten: a fork that lands while another test holds
/// a TCP listener duplicates that fd until the child execs. Plumbing variables
/// are cleared for the same reason as in `intent.rs` — a hook fired inside git
/// tooling must not redirect the command away from `-C`.
fn git_output(cwd: &Path, args: &[&OsStr]) -> Option<Output> {
    #[cfg(test)]
    let _fork_guard = crate::store::port_test_support::lock();
    Command::new("git")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .ok()
}

fn git_bytes(cwd: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let args: Vec<&OsStr> = args.iter().map(|arg| OsStr::new(*arg)).collect();
    let output = git_output(cwd, &args)?;
    output.status.success().then_some(output.stdout)
}

/// Runs a one-line git query, returning stdout with trailing newlines removed.
/// Empty output means "no answer".
fn git_line(cwd: &Path, args: &[&str]) -> Option<String> {
    let stdout = git_bytes(cwd, args)?;
    let text = String::from_utf8_lossy(&stdout);
    let trimmed = text.trim_end_matches(['\r', '\n']);
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Renders a path the way a git tree spec expects it: `/`-separated on every
/// platform. An absolute path (the outside-a-repo fallback) is passed through;
/// git then reports it as absent from the tree, which is the normal no-base
/// case.
fn forward_slashes(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A repository plus one linked worktree, both containing `src/auth.rs`
    /// from the initial commit. Mirrors the fixture in `intent.rs`.
    struct GitRepo {
        _root: tempfile::TempDir,
        main: PathBuf,
        linked: PathBuf,
    }

    /// Runs a git command for the fixture. The fork lock is taken for the same
    /// reason the product's git helper takes it: a child that execs while
    /// another test holds a listener duplicates that fd until the exec, which
    /// makes the socket assertions flaky.
    fn run_git(dir: &Path, args: &[&str]) {
        let _fork_guard = crate::store::port_test_support::lock();
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("git must be installed to run the conflict tests");
        assert!(
            output.status.success(),
            "git {args:?} failed in {}: {}",
            dir.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_repo_with_worktree() -> GitRepo {
        let root = tempfile::tempdir().unwrap();
        let main = root.path().join("main");
        fs::create_dir(&main).unwrap();
        run_git(&main, &["init", "--quiet"]);
        fs::create_dir_all(main.join("src")).unwrap();
        fs::write(main.join("src/auth.rs"), "a\nb\nc\n").unwrap();
        run_git(&main, &["add", "."]);
        // Identity and signing flags are passed explicitly so a developer's
        // global git config cannot make the fixture commit fail.
        run_git(
            &main,
            &[
                "-c",
                "user.email=portzilla@example.com",
                "-c",
                "user.name=Portzilla Test",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "-m",
                "init",
            ],
        );
        let linked = root.path().join("linked");
        run_git(
            &main,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "linked-worktree",
                linked.to_str().unwrap(),
            ],
        );
        GitRepo {
            _root: root,
            main,
            linked,
        }
    }

    /// The holder's location for `rel` in the fixture's linked worktree.
    fn location(repo: &GitRepo, rel: &str) -> IntentLocation {
        IntentLocation {
            file: repo.linked.join(rel),
            worktree: repo.linked.clone(),
            rel: PathBuf::from(rel),
        }
    }

    #[test]
    fn disjoint_edits_are_clean() {
        let repo = git_repo_with_worktree();
        // The holder commits a change on line 1...
        fs::write(repo.linked.join("src/auth.rs"), "A\nb\nc\n").unwrap();
        run_git(&repo.linked, &["add", "."]);
        run_git(
            &repo.linked,
            &[
                "-c",
                "user.email=portzilla@example.com",
                "-c",
                "user.name=Portzilla Test",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "-m",
                "holder change",
            ],
        );

        // ...and the caller edits line 3.
        let verdict = check(
            &repo.main,
            &location(&repo, "src/auth.rs"),
            PendingEdit::Replace {
                old: "c\n",
                new: "C\n",
            },
        );
        assert_eq!(verdict, ConflictVerdict::Clean);
    }

    #[test]
    fn same_line_edits_conflict() {
        let repo = git_repo_with_worktree();
        fs::write(repo.linked.join("src/auth.rs"), "a\nHOLDER\nc\n").unwrap();

        let verdict = check(
            &repo.main,
            &location(&repo, "src/auth.rs"),
            PendingEdit::Replace {
                old: "b\n",
                new: "CALLER\n",
            },
        );
        assert_eq!(verdict, ConflictVerdict::Conflict);
    }

    #[test]
    fn a_file_new_on_both_sides_conflicts_unless_identical() {
        let repo = git_repo_with_worktree();
        fs::write(repo.linked.join("src/new.rs"), "holder\n").unwrap();
        let holder = location(&repo, "src/new.rs");

        // No base blob: the file exists in neither HEAD. Different versions
        // cannot be proven to merge, so they are a conflict.
        let conflict = check(
            &repo.main,
            &holder,
            PendingEdit::Write {
                content: "caller\n",
            },
        );
        assert_eq!(conflict, ConflictVerdict::Conflict);

        // Byte-identical versions have nothing to merge, so they are clean.
        let clean = check(
            &repo.main,
            &holder,
            PendingEdit::Write {
                content: "holder\n",
            },
        );
        assert_eq!(clean, ConflictVerdict::Clean);
    }

    #[test]
    fn a_missing_holder_file_is_unprovable() {
        let repo = git_repo_with_worktree();
        let verdict = check(
            &repo.main,
            &location(&repo, "src/missing.rs"),
            PendingEdit::Write {
                content: "anything\n",
            },
        );
        assert_eq!(verdict, ConflictVerdict::Unprovable);
    }

    #[test]
    fn an_edit_whose_old_text_is_absent_is_unprovable() {
        let repo = git_repo_with_worktree();
        // The holder's file exists (checked out by `worktree add`), so the
        // failure is the payload disagreeing with the caller's file.
        let verdict = check(
            &repo.main,
            &location(&repo, "src/auth.rs"),
            PendingEdit::Replace {
                old: "not in the file\n",
                new: "x\n",
            },
        );
        assert_eq!(verdict, ConflictVerdict::Unprovable);
    }

    #[test]
    fn a_write_is_compared_as_the_whole_new_file() {
        let repo = git_repo_with_worktree();
        let holder = location(&repo, "src/auth.rs");
        fs::write(repo.linked.join("src/auth.rs"), "A\nb\nc\n").unwrap();

        // A whole-file write that only touches line 3 merges with the holder's
        // line 1 change...
        let clean = check(
            &repo.main,
            &holder,
            PendingEdit::Write {
                content: "a\nb\nC\n",
            },
        );
        assert_eq!(clean, ConflictVerdict::Clean);

        // ...while a whole-file write that touches the holder's line conflicts.
        let conflict = check(
            &repo.main,
            &holder,
            PendingEdit::Write {
                content: "X\nb\nc\n",
            },
        );
        assert_eq!(conflict, ConflictVerdict::Conflict);
    }
}
