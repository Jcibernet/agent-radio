//! Conformance suite for the store format and CLI contract.
//!
//! Ported 1:1 from the original Python implementation's tests; any
//! implementation of the protocol must pass these against its binary.

use std::io::Write;
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

    fn run(&self, args: &[&str]) -> Output {
        self.cmd().args(args).output().unwrap()
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

    fn fails(&self, args: &[&str]) -> Output {
        let out = self.run(args);
        assert!(!out.status.success(), "expected failure for {args:?}");
        out
    }

    fn messages(&self) -> Vec<Value> {
        let path = self.dir.path().join("messages.jsonl");
        if !path.exists() {
            return Vec::new();
        }
        std::fs::read_to_string(path)
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
