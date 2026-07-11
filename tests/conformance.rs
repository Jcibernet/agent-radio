//! Conformance suite for the store format and CLI contract.
//!
//! Ported 1:1 from the original Python implementation's tests; any
//! implementation of the protocol must pass these against its binary.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::Value;
use tempfile::TempDir;

struct Radio {
    dir: TempDir,
}

impl Radio {
    fn new() -> Self {
        Self {
            dir: TempDir::new().unwrap(),
        }
    }

    fn cmd(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_agent-radio"));
        c.env("AGENT_RADIO_DIR", self.dir.path());
        c.env_remove("AGENT_RADIO_AGENT");
        c
    }

    fn cmd_in(&self, dir: &Path) -> Command {
        let mut c = self.cmd();
        c.current_dir(dir);
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd().args(args).output().unwrap()
    }

    fn run_in(&self, args: &[&str], dir: &Path) -> Output {
        self.cmd_in(dir).args(args).output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "expected success for {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn ok_in(&self, args: &[&str], dir: &Path) -> String {
        let out = self.cmd_in(dir).args(args).output().unwrap();
        assert!(
            out.status.success(),
            "expected success for {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn fails(&self, args: &[&str]) -> Output {
        let out = self.run(args);
        assert!(!out.status.success(), "expected failure for {args:?}");
        out
    }

    fn init_git(&self, dir: &Path) {
        let out = Command::new("git")
            .args(["init"])
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "git init failed");
        let out = Command::new("git")
            .args(["config", "user.email", "t@t"])
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(out.status.success());
        let out = Command::new("git")
            .args(["config", "user.name", "t"])
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(out.status.success());
        fs::write(dir.join(".gitkeep"), b"").unwrap();
        let out = Command::new("git")
            .args(["add", "."])
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(out.status.success());
        let out = Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(out.status.success());
    }

    fn work_dir(&self) -> PathBuf {
        let wd = self.dir.path().join("repo");
        fs::create_dir_all(&wd).unwrap();
        self.init_git(&wd);
        wd
    }

    fn messages(&self) -> Vec<Value> {
        let path = self.dir.path().join("messages.jsonl");
        if !path.exists() {
            return Vec::new();
        }
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }
}

fn send(r: &Radio, from: &str, to: &str, body: &str) {
    r.ok(&[
        "send", "--from", from, "--to", to, "--body", body, "--branch", "",
    ]);
}

// ------------------------------------------------------------ store/env --

#[test]
fn store_honors_agent_radio_dir_outside_git() {
    let r = Radio::new();
    // cwd = / (outside any git worktree); AGENT_RADIO_DIR must be enough.
    let out = r.cmd().current_dir("/").args(["team"]).output().unwrap();
    assert!(out.status.success());
}

#[test]
fn send_writes_message_and_blank_branch_is_omitted() {
    let r = Radio::new();
    let out = r.ok(&[
        "send", "--from", "a", "--to", "b", "--kind", "FYI", "--body", "hello", "--branch", "",
    ]);
    assert!(out.contains("sent FYI a -> b"));
    let msgs = r.messages();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0]["body"], "hello");
    assert_eq!(msgs[0]["version"], 1);
    assert!(msgs[0].get("branch").is_none());
}

#[test]
fn identity_from_env_var() {
    let r = Radio::new();
    let mut c = r.cmd();
    c.env("AGENT_RADIO_AGENT", "envbot");
    let out = c
        .args(["send", "--to", "b", "--body", "hi", "--branch", ""])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(r.messages()[0]["from"], "envbot");
}

#[test]
fn missing_identity_fails() {
    let r = Radio::new();
    r.fails(&["send", "--to", "b", "--body", "hi", "--branch", ""]);
}

// ----------------------------------------------------------------- flow --

#[test]
fn inbox_marks_read_and_peek_does_not() {
    let r = Radio::new();
    send(&r, "a", "b", "q1");
    let peeked = r.ok(&["inbox", "--as", "b", "--peek"]);
    assert!(peeked.contains("q1"));
    let again = r.ok(&["inbox", "--as", "b"]);
    assert!(again.contains("q1"));
    let empty = r.ok(&["inbox", "--as", "b"]);
    assert!(empty.contains("empty"));
}

#[test]
fn reply_threads_and_targets_sender() {
    let r = Radio::new();
    r.ok(&[
        "send", "--from", "a", "--to", "b", "--kind", "ASK", "--body", "can you?", "--branch", "",
    ]);
    r.ok(&["inbox", "--as", "b"]);
    r.ok(&["done", "1", "--as", "b", "--body", "shipped"]);
    let msgs = r.messages();
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[1]["kind"], "DONE");
    assert_eq!(msgs[1]["to"], "a");
    assert_eq!(msgs[1]["reply_to"], msgs[0]["id"]);
    assert_eq!(msgs[1]["thread_id"], msgs[0]["id"]);
}

#[test]
fn broadcast_reaches_known_agents_not_sender() {
    let r = Radio::new();
    send(&r, "a", "b", "x");
    send(&r, "a", "all", "heads up");
    let b_inbox = r.ok(&["inbox", "--as", "b"]);
    assert!(b_inbox.contains("heads up"));
    let a_inbox = r.ok(&["inbox", "--as", "a"]);
    assert!(a_inbox.contains("empty"));
}

#[test]
fn status_counts_unread_and_quiet_exit_codes() {
    let r = Radio::new();
    send(&r, "a", "b", "x");
    let status: Value = serde_json::from_str(&r.ok(&["status", "--as", "b"])).unwrap();
    assert_eq!(status["unread"], 1);
    assert!(r.run(&["status", "--as", "b", "--quiet"]).status.success());
    assert!(!r.run(&["status", "--as", "a", "--quiet"]).status.success());
}

// ------------------------------------------------------------- sanitize --

#[test]
fn render_neutralises_csi_and_osc_sequences() {
    let r = Radio::new();
    send(&r, "a", "b", "before\x1b[2Jafter");
    send(&r, "a", "b", "term\x1b]0;owned title\x07message");
    let out = r.ok(&["inbox", "--as", "b"]);
    assert!(out.contains("before[2Jafter"));
    assert!(out.contains("term]0;owned titlemessage"));
    assert!(!out.chars().any(|c| c.is_control() && c != '\n'));
}

#[test]
fn render_neutralises_overwrite_and_control_chars() {
    let r = Radio::new();
    send(&r, "a", "b", "safe line\rforged line");
    send(&r, "a", "b", "abc\x08\x08X");
    // NUL cannot travel in argv (OS limitation), so it goes via stdin —
    // same store, same render path.
    let out = run_with_stdin(
        &r,
        &[
            "send", "--from", "a", "--to", "b", "--body", "-", "--branch", "",
        ],
        "a\u{0000}b\u{007f}c\u{009b}31md\u{0085}e",
    );
    assert!(out.status.success());
    let rendered = r.ok(&["inbox", "--as", "b"]);
    assert!(rendered.contains("safe line forged line"));
    assert!(rendered.contains("abcX"));
    assert!(rendered.contains("abc31mde"));
    assert!(!rendered.chars().any(|c| c.is_control() && c != '\n'));
}

#[test]
fn rendered_output_never_contains_controls_in_any_field() {
    let r = Radio::new();
    r.ok(&[
        "send",
        "--from",
        "a",
        "--to",
        "b",
        "--kind",
        "FYI",
        "--body",
        "evil\x1b[2J\x07body",
        "--risk",
        "r\x1bisk",
        "--focus",
        "f\x07ile.py",
        "--branch",
        "",
    ]);
    let out = r.ok(&["inbox", "--as", "b"]);
    assert!(out.contains("file.py"));
    assert!(!out.chars().any(|c| c.is_control() && c != '\n'));
}

// ---------------------------------------------------------------- stdin --

fn run_with_stdin(r: &Radio, args: &[&str], input: &str) -> Output {
    let mut child = r
        .cmd()
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn send_body_dash_reads_stdin() {
    let r = Radio::new();
    let out = run_with_stdin(
        &r,
        &[
            "send", "--from", "a", "--to", "b", "--body", "-", "--branch", "",
        ],
        "multi 'quoted' body\nfrom stdin",
    );
    assert!(out.status.success());
    let msgs = r.messages();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0]["body"], "multi 'quoted' body\nfrom stdin");
}

#[test]
fn reply_body_dash_reads_stdin() {
    let r = Radio::new();
    r.ok(&[
        "send", "--from", "a", "--to", "b", "--kind", "ASK", "--body", "q", "--branch", "",
    ]);
    r.ok(&["inbox", "--as", "b"]);
    let out = run_with_stdin(
        &r,
        &["done", "1", "--as", "b", "--body", "-"],
        "done via stdin",
    );
    assert!(out.status.success());
    assert_eq!(r.messages()[1]["body"], "done via stdin");
}

// --------------------------------------------------------------- guards --

#[test]
fn secret_looking_body_is_rejected() {
    let r = Radio::new();
    r.fails(&[
        "send",
        "--from",
        "a",
        "--to",
        "b",
        "--branch",
        "",
        "--body",
        "api_key = 'sk-proj-abcdefghijklmnopqrstuvwxyz0123456789ABCD'",
    ]);
    assert!(r.messages().is_empty());
}

#[test]
fn invalid_kind_rejected() {
    let r = Radio::new();
    r.fails(&[
        "send", "--from", "a", "--to", "b", "--kind", "NOPE", "--body", "x", "--branch", "",
    ]);
}

#[test]
fn invalid_agent_name_rejected() {
    let r = Radio::new();
    r.fails(&[
        "send",
        "--from",
        "bad name!",
        "--to",
        "b",
        "--body",
        "x",
        "--branch",
        "",
    ]);
}

// -------------------------------------------------------------- history --

#[test]
fn history_filters_and_saves_view_numbering() {
    let r = Radio::new();
    send(&r, "a", "b", "first");
    send(&r, "a", "c", "second");
    send(&r, "b", "a", "third");
    let with_b = r.ok(&["history", "--with", "b"]);
    assert!(with_b.contains("first") && with_b.contains("third"));
    assert!(!with_b.contains("second"));
    let limited = r.ok(&["history", "--limit", "1"]);
    assert!(limited.contains("third") && !limited.contains("first"));
    // --as saves numbering: reply to #1 of the filtered view
    r.ok(&["history", "--as", "c", "--with", "c"]);
    r.ok(&["ack", "1", "--as", "c"]);
    let last = r.messages().pop().unwrap();
    assert_eq!(last["kind"], "ACK");
    assert_eq!(last["to"], "a");
    assert_eq!(last["body"], "ack");
}

#[test]
fn wait_times_out_with_exit_1() {
    let r = Radio::new();
    let out = r.run(&[
        "wait",
        "--as",
        "x",
        "--timeout",
        "0.1",
        "--interval",
        "0.05",
    ]);
    assert!(!out.status.success());
}

// ------------------------------------------------------------ manifests --

#[test]
fn manifest_digest_deterministic() {
    let r = Radio::new();
    let wd = r.work_dir();
    let hello_sha256 = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
    fs::write(wd.join("a.txt"), b"hello").unwrap();
    let out = r.ok_in(&["manifest", "emit", "a.txt"], &wd);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["files"]["a.txt"], hello_sha256);
    assert!(v["digest"].as_str().unwrap().len() == 64);
    let digest1 = v["digest"].as_str().unwrap().to_string();
    let out2 = r.ok_in(&["manifest", "emit", "a.txt"], &wd);
    let v2: serde_json::Value = serde_json::from_str(&out2).unwrap();
    assert_eq!(v2["digest"].as_str().unwrap(), digest1);
    assert!(out.contains("generated_at"));
}

#[test]
fn manifest_emit_uses_git_status() {
    let r = Radio::new();
    let wd = r.work_dir();
    fs::write(wd.join("new.txt"), b"content").unwrap();
    let out = r.ok_in(&["manifest", "emit"], &wd);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(v["files"].as_object().unwrap().contains_key("new.txt"));
}

#[test]
fn manifest_done_with_manifest_embeds() {
    let r = Radio::new();
    let wd = r.work_dir();
    fs::write(wd.join("task.txt"), b"proof").unwrap();
    r.ok_in(
        &[
            "send",
            "--from",
            "bot",
            "--to",
            "human",
            "--kind",
            "DONE",
            "--body",
            "done with manifest",
            "--branch",
            "",
            "--task",
            "task-42",
            "--manifest",
            "task.txt",
        ],
        &wd,
    );
    let msgs = r.messages();
    assert_eq!(msgs.len(), 1);
    let m = &msgs[0];
    assert_eq!(m["task"], "task-42");
    assert!(m.get("manifest").is_some());
    let files = m["manifest"]["files"].as_object().unwrap();
    assert!(files.contains_key("task.txt"));
    assert!(m["manifest"]["digest"].as_str().unwrap().len() == 64);
}

#[test]
fn manifest_verify_ok() {
    let r = Radio::new();
    let wd = r.work_dir();
    fs::write(wd.join("verify.txt"), b"stable").unwrap();
    r.ok_in(
        &[
            "send",
            "--from",
            "bot",
            "--to",
            "human",
            "--body",
            "check",
            "--branch",
            "",
            "--manifest",
            "verify.txt",
        ],
        &wd,
    );
    r.ok_in(&["history", "--as", "human"], &wd);
    let out = r.ok_in(&["manifest", "verify", "1", "--as", "human"], &wd);
    assert!(out.contains("OK"));
    assert!(out.contains("VERIFICADO"));
}

#[test]
fn manifest_verify_mismatch() {
    let r = Radio::new();
    let wd = r.work_dir();
    fs::write(wd.join("alter.txt"), b"original").unwrap();
    r.ok_in(
        &[
            "send",
            "--from",
            "bot",
            "--to",
            "human",
            "--body",
            "check",
            "--branch",
            "",
            "--manifest",
            "alter.txt",
        ],
        &wd,
    );
    fs::write(wd.join("alter.txt"), b"modified").unwrap();
    r.ok_in(&["history", "--as", "human"], &wd);
    let out = r.run_in(&["manifest", "verify", "1", "--as", "human"], &wd);
    assert!(!out.status.success());
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("MISMATCH") || stderr.contains("MISMATCH"));
}

#[test]
fn manifest_verify_missing() {
    let r = Radio::new();
    let wd = r.work_dir();
    fs::write(wd.join("gone.txt"), b"temp").unwrap();
    r.ok_in(
        &[
            "send",
            "--from",
            "bot",
            "--to",
            "human",
            "--body",
            "check",
            "--branch",
            "",
            "--manifest",
            "gone.txt",
        ],
        &wd,
    );
    fs::remove_file(wd.join("gone.txt")).unwrap();
    r.ok_in(&["history", "--as", "human"], &wd);
    let out = r.run_in(&["manifest", "verify", "1", "--as", "human"], &wd);
    assert!(!out.status.success());
    assert_eq!(out.status.code(), Some(2));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("MISSING"));
}

#[test]
fn manifest_verify_strict_orphan() {
    let r = Radio::new();
    let wd = r.work_dir();
    fs::write(wd.join("claimed.txt"), b"mine").unwrap();
    r.ok_in(
        &[
            "send",
            "--from",
            "bot",
            "--to",
            "human",
            "--body",
            "check",
            "--branch",
            "",
            "--manifest",
            "claimed.txt",
        ],
        &wd,
    );
    fs::write(wd.join("orphan.txt"), b"unclaimed").unwrap();
    r.ok_in(&["history", "--as", "human"], &wd);
    let out = r.run_in(
        &["manifest", "verify", "1", "--as", "human", "--strict"],
        &wd,
    );
    assert!(!out.status.success());
    assert_eq!(out.status.code(), Some(3));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("HUERFANO") || stderr.contains("orphan.txt"));
}

#[test]
fn manifest_map_dedup_by_task() {
    let r = Radio::new();
    let wd = r.work_dir();
    fs::write(wd.join("a.txt"), b"first").unwrap();
    r.ok_in(
        &[
            "send",
            "--from",
            "bot",
            "--to",
            "human",
            "--body",
            "first",
            "--branch",
            "",
            "--task",
            "shared-task",
            "--manifest",
            "a.txt",
        ],
        &wd,
    );
    fs::write(wd.join("b.txt"), b"second").unwrap();
    r.ok_in(
        &[
            "send",
            "--from",
            "bot",
            "--to",
            "human",
            "--body",
            "second",
            "--branch",
            "",
            "--task",
            "shared-task",
            "--manifest",
            "b.txt",
        ],
        &wd,
    );
    let out = r.ok_in(&["manifest", "map", "--limit", "10"], &wd);
    assert!(out.contains("shared-task"));
    assert!(out.contains("b.txt"));
}

#[test]
fn manifest_backward_compat_messages_without_manifest() {
    let r = Radio::new();
    send(&r, "a", "b", "plain message");
    let out = r.ok(&["inbox", "--as", "b"]);
    assert!(out.contains("plain message"));
    assert!(!out.contains("[manifest"));
}

// ---------------------------------------------------- contract gap tests --

/// Defends the core claim: append-only JSONL + flock keeps concurrent
/// writers safe. Every parallel send must land as one intact line.
#[test]
fn concurrent_sends_never_corrupt_the_store() {
    let r = Radio::new();
    let n = 12;
    let children: Vec<_> = (0..n)
        .map(|i| {
            let mut c = r.cmd();
            c.args([
                "send",
                "--from",
                "a",
                "--to",
                "b",
                "--branch",
                "",
                "--body",
                &format!("parallel message {i}"),
            ]);
            c.stdout(Stdio::null()).stderr(Stdio::piped());
            c.spawn().unwrap()
        })
        .collect();
    for child in children {
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "concurrent send failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let msgs = r.messages();
    assert_eq!(msgs.len(), n, "every send must land as exactly one line");
    let mut bodies: Vec<String> = msgs
        .iter()
        .map(|m| m["body"].as_str().unwrap().to_string())
        .collect();
    bodies.sort();
    let mut expected: Vec<String> = (0..n).map(|i| format!("parallel message {i}")).collect();
    expected.sort();
    assert_eq!(bodies, expected);
}

/// The store format is the compatibility contract: a messages.jsonl written
/// verbatim by the original Python implementation (space-separated JSON,
/// sorted keys) must read and reply identically.
#[test]
fn reads_stores_written_by_the_python_implementation() {
    let r = Radio::new();
    let fixture = concat!(
        "{\"body\": \"hola desde python\", \"from\": \"py\", \"id\": \"963ec95d78f54ceb\", ",
        "\"kind\": \"ASK\", \"to\": \"rs\", \"ts\": \"2026-07-04T20:46:20.443053Z\", \"version\": 1}\n",
        "{\"body\": \"full fields\", \"branch\": \"dev\", \"focus\": [\"src/a.rs\", \"src/b.rs\"], ",
        "\"from\": \"py\", \"id\": \"aaaabbbbccccdddd\", \"kind\": \"FYI\", \"priority\": \"high\", ",
        "\"reply_to\": \"963ec95d78f54ceb\", \"risk\": \"none\", \"thread_id\": \"963ec95d78f54ceb\", ",
        "\"to\": \"rs\", \"ts\": \"2026-07-04T20:46:21.000000Z\", \"version\": 1}\n",
    );
    std::fs::write(r.dir.path().join("messages.jsonl"), fixture).unwrap();

    let inbox = r.ok(&["inbox", "--as", "rs"]);
    assert!(inbox.contains("hola desde python"));
    assert!(inbox.contains("full fields"));
    assert!(inbox.contains("focus: src/a.rs, src/b.rs"));
    let history = r.ok(&["history"]);
    assert!(history.contains("963ec95d78f54ceb"));

    r.ok(&["done", "1", "--as", "rs", "--body", "leido"]);
    let reply = r.messages().pop().unwrap();
    assert_eq!(reply["to"], "py");
    assert_eq!(reply["reply_to"], "963ec95d78f54ceb");
    assert_eq!(
        reply["branch"].as_str(),
        None,
        "fixture msg #1 has no branch"
    );
}

/// Replies must thread to the ROOT message id, not the intermediate reply.
#[test]
fn thread_id_survives_reply_chains() {
    let r = Radio::new();
    r.ok(&[
        "send", "--from", "a", "--to", "b", "--kind", "ASK", "--body", "root ask", "--branch", "",
    ]);
    let root_id = r.messages()[0]["id"].as_str().unwrap().to_string();
    r.ok(&["inbox", "--as", "b"]);
    r.ok(&["done", "1", "--as", "b", "--body", "reply one"]);
    let done_id = r.messages()[1]["id"].as_str().unwrap().to_string();

    r.ok(&["inbox", "--as", "a"]);
    r.ok(&["ack", "1", "--as", "a", "--body", "thanks"]);
    let ack = r.messages().pop().unwrap();
    assert_eq!(ack["reply_to"], done_id.as_str());
    assert_eq!(
        ack["thread_id"],
        root_id.as_str(),
        "thread must point at the root, not the intermediate reply"
    );
}

/// Replying to a message you sent must target the recipient, not yourself.
#[test]
fn replying_to_own_message_targets_recipient() {
    let r = Radio::new();
    r.ok(&[
        "send", "--from", "a", "--to", "b", "--kind", "ASK", "--body", "mine", "--branch", "",
    ]);
    r.ok(&["history", "--as", "a"]);
    r.ok(&["ack", "1", "--as", "a", "--body", "self follow-up"]);
    let reply = r.messages().pop().unwrap();
    assert_eq!(reply["to"], "b");
    assert_eq!(reply["from"], "a");
}
