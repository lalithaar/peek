//! Git actions: stage and commit the files you actually touched, and push.
//!
//! Shells out to your own `git`, so your identity, hooks, aliases and
//! credential helper behave exactly as they do in a terminal. Nothing here
//! touches credentials, rewrites history or runs anything destructive.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::Sender;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// Add + commit the file currently open.
    CommitCurrent,
    /// Add + commit every file edited during this session.
    CommitAll,
    /// Push the current branch.
    Push,
}

#[derive(Debug)]
pub struct Outcome {
    pub op: Op,
    pub ok: bool,
    pub message: String,
}

pub fn spawn(root: PathBuf, op: Op, paths: Vec<PathBuf>, tx: Sender<Outcome>) {
    std::thread::spawn(move || {
        let _ = tx.send(run(&root, op, &paths));
    });
}

/// Runs a git action synchronously. `paths` are absolute and only matter for
/// the commit ops.
pub fn run(root: &Path, op: Op, paths: &[PathBuf]) -> Outcome {
    match op {
        Op::Push => push(root),
        Op::CommitCurrent | Op::CommitAll => commit(root, op, paths),
    }
}

fn commit(root: &Path, op: Op, paths: &[PathBuf]) -> Outcome {
    if paths.is_empty() {
        return fail(op, "no files to commit");
    }
    let top = match repo_root(root) {
        Ok(top) => top,
        Err(err) => return fail(op, &err),
    };

    // Stage exactly the requested paths. `git commit -- <paths>` below then
    // commits only those, leaving anything else you already had staged alone.
    let mut add = vec!["add".to_string(), "--".to_string()];
    add.extend(paths.iter().map(|p| arg(p)));
    if let Err(err) = git(&top, &add) {
        return fail(op, &format!("add failed: {err}"));
    }

    let mut stat = vec![
        "diff".to_string(),
        "--cached".to_string(),
        "--numstat".to_string(),
        "--".to_string(),
    ];
    stat.extend(paths.iter().map(|p| arg(p)));
    let numstat = git(&top, &stat).unwrap_or_default();
    let (insertions, deletions, changed) = parse_numstat(&numstat);
    if changed == 0 {
        return fail(op, "those files have no changes");
    }

    let subject = subject_line(root, paths, insertions, deletions);
    let message = commit_message(root, paths, &subject);

    let mut commit = vec!["commit".to_string(), "-m".to_string(), message];
    commit.push("--".to_string());
    commit.extend(paths.iter().map(|p| arg(p)));
    if let Err(err) = git(&top, &commit) {
        return fail(op, &format!("commit failed: {err}"));
    }

    Outcome {
        op,
        ok: true,
        message: format!("committed {subject}"),
    }
}

fn push(root: &Path) -> Outcome {
    let top = match repo_root(root) {
        Ok(top) => top,
        Err(err) => return fail(Op::Push, &err),
    };
    let branch = git(
        &top,
        &[
            "rev-parse".to_string(),
            "--abbrev-ref".to_string(),
            "HEAD".to_string(),
        ],
    )
    .unwrap_or_else(|_| "HEAD".to_string());

    let has_upstream = git(
        &top,
        &[
            "rev-parse".to_string(),
            "--abbrev-ref".to_string(),
            "--symbolic-full-name".to_string(),
            "@{u}".to_string(),
        ],
    )
    .is_ok();

    let result = if has_upstream {
        git(&top, &["push".to_string()])
    } else {
        // First push of a new branch: give it an upstream instead of failing.
        git(
            &top,
            &[
                "push".to_string(),
                "-u".to_string(),
                "origin".to_string(),
                "HEAD".to_string(),
            ],
        )
    };

    match result {
        Ok(_) if has_upstream => Outcome {
            op: Op::Push,
            ok: true,
            message: format!("pushed {branch}"),
        },
        Ok(_) => Outcome {
            op: Op::Push,
            ok: true,
            message: format!("pushed {branch} (set upstream to origin)"),
        },
        Err(err) => fail(Op::Push, &format!("push failed: {err}")),
    }
}

fn fail(op: Op, message: &str) -> Outcome {
    Outcome {
        op,
        ok: false,
        message: format!("git: {message}"),
    }
}

fn repo_root(dir: &Path) -> Result<PathBuf, String> {
    match git(
        dir,
        &["rev-parse".to_string(), "--show-toplevel".to_string()],
    ) {
        Ok(path) => Ok(PathBuf::from(path)),
        Err(err) => Err(format!("not a git repository ({err})")),
    }
}

fn git(dir: &Path, args: &[String]) -> Result<String, String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|err| format!("cannot run git: {err}"))?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).trim().to_string());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let line = stderr
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("git failed")
        .trim()
        .to_string();
    Err(line)
}

fn arg(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

/// Path as the user sees it: relative to the folder peek was opened on.
fn display_path(root: &Path, path: &Path) -> String {
    let relative = path.strip_prefix(root).unwrap_or(path);
    relative.to_string_lossy().replace('\\', "/")
}

/// `update 3 files (+40 -9)` — counts omitted when git reports none.
fn subject_line(root: &Path, paths: &[PathBuf], insertions: u32, deletions: u32) -> String {
    let counts = if insertions == 0 && deletions == 0 {
        String::new()
    } else {
        format!(" (+{insertions} -{deletions})")
    };
    if paths.len() == 1 {
        format!("update {}{counts}", display_path(root, &paths[0]))
    } else {
        format!("update {} files{counts}", paths.len())
    }
}

fn commit_message(root: &Path, paths: &[PathBuf], subject: &str) -> String {
    let mut message = format!("peek: {subject}");
    if paths.len() > 1 {
        message.push_str("\n\n");
        for path in paths {
            message.push_str(&format!("- {}\n", display_path(root, path)));
        }
        message.pop();
    }
    message
}

/// `git diff --numstat` lines are `ins\tdel\tpath`; binary files use `-`.
fn parse_numstat(text: &str) -> (u32, u32, usize) {
    let mut insertions = 0u32;
    let mut deletions = 0u32;
    let mut files = 0usize;
    for line in text.lines() {
        let mut parts = line.split('\t');
        let (Some(ins), Some(del), Some(_path)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        files += 1;
        insertions += ins.parse::<u32>().unwrap_or(0);
        deletions += del.parse::<u32>().unwrap_or(0);
    }
    (insertions, deletions, files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false)
    }

    fn temp_repo(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("peek-git-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "peek@example.invalid"],
            vec!["config", "user.name", "peek"],
            vec!["config", "commit.gpgsign", "false"],
            vec!["config", "core.autocrlf", "false"],
        ] {
            let out = Command::new("git")
                .args(&args)
                .current_dir(&dir)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?} failed");
        }
        dir
    }

    fn git_out(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn subject_and_counts_read_naturally() {
        let root = Path::new("D:/proj");
        let one = vec![PathBuf::from("D:/proj/src/index.rs")];
        assert_eq!(
            subject_line(root, &one, 12, 3),
            "update src/index.rs (+12 -3)"
        );

        let many = vec![
            PathBuf::from("D:/proj/a.rs"),
            PathBuf::from("D:/proj/b.rs"),
            PathBuf::from("D:/proj/c.rs"),
        ];
        assert_eq!(subject_line(root, &many, 40, 9), "update 3 files (+40 -9)");
        assert_eq!(subject_line(root, &many, 0, 0), "update 3 files");
    }

    #[test]
    fn multi_file_commit_gets_a_body_listing_the_files() {
        let root = Path::new("D:/proj");
        let paths = vec![
            PathBuf::from("D:/proj/a.rs"),
            PathBuf::from("D:/proj/src/b.rs"),
        ];
        let message = commit_message(root, &paths, "update 2 files (+5 -1)");
        assert_eq!(
            message,
            "peek: update 2 files (+5 -1)\n\n- a.rs\n- src/b.rs"
        );

        let single = commit_message(root, &paths[..1], "update a.rs (+1 -0)");
        assert_eq!(single, "peek: update a.rs (+1 -0)");
    }

    #[test]
    fn numstat_sums_insertions_and_ignores_binary_counts() {
        let text = "12\t3\tsrc/a.rs\n-\t-\tlogo.png\n0\t7\tsrc/b.rs\n";
        assert_eq!(parse_numstat(text), (12, 10, 3));
        assert_eq!(parse_numstat(""), (0, 0, 0));
    }

    #[test]
    fn commits_only_the_given_file() {
        if !git_available() {
            eprintln!("git not available; skipping");
            return;
        }
        let dir = temp_repo("only");
        let a = dir.join("a.txt");
        let b = dir.join("b.txt");
        fs::write(&a, "one\n").unwrap();
        fs::write(&b, "two\n").unwrap();

        // Track b first so both files are known to git.
        let first = run(&dir, Op::CommitCurrent, std::slice::from_ref(&b));
        assert!(first.ok, "{}", first.message);

        fs::write(&a, "one changed\n").unwrap();
        fs::write(&b, "two changed\n").unwrap();

        let second = run(&dir, Op::CommitCurrent, std::slice::from_ref(&a));
        assert!(second.ok, "{}", second.message);

        let status = git_out(&dir, &["status", "--porcelain"]);
        assert!(!status.contains("a.txt"), "a should be committed: {status}");
        assert!(
            status.contains("b.txt"),
            "b must stay uncommitted: {status}"
        );

        let subject = git_out(&dir, &["log", "-1", "--format=%s"]);
        assert!(subject.starts_with("peek: update a.txt"), "{subject}");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn commits_all_given_files_in_one_commit() {
        if !git_available() {
            eprintln!("git not available; skipping");
            return;
        }
        let dir = temp_repo("all");
        let a = dir.join("a.txt");
        let b = dir.join("b.txt");
        fs::write(&a, "a\n").unwrap();
        fs::write(&b, "b\n").unwrap();

        let outcome = run(&dir, Op::CommitAll, &[a.clone(), b.clone()]);
        assert!(outcome.ok, "{}", outcome.message);

        let touched = git_out(&dir, &["show", "--name-only", "--format=", "HEAD"]);
        assert!(
            touched.contains("a.txt") && touched.contains("b.txt"),
            "{touched}"
        );
        assert_eq!(git_out(&dir, &["status", "--porcelain"]), "");

        let body = git_out(&dir, &["log", "-1", "--format=%B"]);
        assert!(body.starts_with("peek: update 2 files"), "{body}");
        assert!(
            body.contains("- a.txt") && body.contains("- b.txt"),
            "{body}"
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unchanged_files_report_that_instead_of_committing() {
        if !git_available() {
            eprintln!("git not available; skipping");
            return;
        }
        let dir = temp_repo("nochange");
        let a = dir.join("a.txt");
        fs::write(&a, "same\n").unwrap();
        assert!(run(&dir, Op::CommitCurrent, std::slice::from_ref(&a)).ok);

        let again = run(&dir, Op::CommitCurrent, std::slice::from_ref(&a));
        assert!(!again.ok);
        assert!(again.message.contains("no changes"), "{}", again.message);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn outside_a_repo_it_says_so() {
        if !git_available() {
            eprintln!("git not available; skipping");
            return;
        }
        let dir = std::env::temp_dir().join(format!("peek-git-plain-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("notes.txt");
        fs::write(&file, "x\n").unwrap();

        let outcome = run(&dir, Op::CommitCurrent, &[file]);
        assert!(!outcome.ok);
        assert!(
            outcome.message.contains("not a git repository"),
            "{}",
            outcome.message
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn push_reports_a_missing_remote_instead_of_hanging() {
        if !git_available() {
            eprintln!("git not available; skipping");
            return;
        }
        let dir = temp_repo("noremote");
        let a = dir.join("a.txt");
        fs::write(&a, "a\n").unwrap();
        assert!(run(&dir, Op::CommitCurrent, &[a]).ok);

        let outcome = run(&dir, Op::Push, &[]);
        assert!(!outcome.ok, "no remote is configured");
        assert!(
            outcome.message.starts_with("git: push failed"),
            "{}",
            outcome.message
        );

        fs::remove_dir_all(&dir).ok();
    }
}
