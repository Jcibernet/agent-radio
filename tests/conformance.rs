//! Conformance suite for the store format and CLI contract.
//!
//! Ported 1:1 from the original Python implementation's tests; any
//! implementation of the protocol must pass these against its binary.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Barrier};
use std::thread;

use serde_json::Value;
use sha2::{Digest, Sha256};

#[cfg(unix)]
use std::os::unix::fs::{symlink, PermissionsExt};
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
        c.env_remove("AGENT_RADIO_CLIENT_ID");
        c.env_remove("AGENT_RADIO_PROVIDER");
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

#[test]
fn registration_assigns_stable_human_names_in_creation_order() {
    let r = Radio::new();
    assert_eq!(
        r.ok(&[
            "register",
            "--client-id",
            "session-alice",
            "--provider",
            "claude"
        ])
        .trim(),
        "Alice"
    );
    assert_eq!(
        r.ok(&["register", "--client-id", "session-alice"]).trim(),
        "Alice"
    );
    assert_eq!(
        r.ok(&[
            "register",
            "--client-id",
            "session-bob",
            "--provider",
            "opencode"
        ])
        .trim(),
        "Bob"
    );

    let team = r.ok(&["team"]);
    let lines: Vec<&str> = team.lines().collect();
    assert!(lines[0].starts_with("Alice\tclaude\t"));
    assert!(lines[1].starts_with("Bob\topencode\t"));
    let registry = fs::read_to_string(r.dir.path().join("agents.json")).unwrap();
    assert!(!registry.contains("session-alice"));
    assert!(!registry.contains("session-bob"));
}

#[test]
fn client_id_identity_routes_messages_end_to_end() {
    let r = Radio::new();
    r.ok(&["register", "--client-id", "session-alice"]);
    r.ok(&["register", "--client-id", "session-bob"]);

    let mut alice = r.cmd();
    alice.env("AGENT_RADIO_CLIENT_ID", "session-alice");
    let sent = alice
        .args([
            "send",
            "--to",
            "Bob",
            "--body",
            "Can you review this?",
            "--branch",
            "",
        ])
        .output()
        .unwrap();
    assert!(sent.status.success());
    assert!(String::from_utf8(sent.stdout)
        .unwrap()
        .contains("Alice -> Bob"));

    let mut bob = r.cmd();
    bob.env("AGENT_RADIO_CLIENT_ID", "session-bob");
    let inbox = bob.args(["inbox"]).output().unwrap();
    assert!(inbox.status.success());
    let rendered = String::from_utf8(inbox.stdout).unwrap();
    assert!(rendered.contains("Alice -> Bob"));
    assert!(rendered.contains("Can you review this?"));
}

#[test]
fn concurrent_registration_never_reuses_a_human_name() {
    let r = Radio::new();
    let store = r.dir.path().to_path_buf();
    let barrier = Arc::new(Barrier::new(8));
    let handles: Vec<_> = (0..8)
        .map(|index| {
            let store = store.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                let output = Command::new(env!("CARGO_BIN_EXE_agent-radio"))
                    .env("AGENT_RADIO_DIR", store)
                    .args(["register", "--client-id", &format!("session-{index}")])
                    .output()
                    .unwrap();
                assert!(output.status.success());
                String::from_utf8(output.stdout).unwrap().trim().to_string()
            })
        })
        .collect();
    let mut names: Vec<String> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();
    names.sort();
    assert_eq!(
        names,
        ["Alice", "Bob", "Charlie", "Diana", "Ethan", "Fiona", "George", "Hannah"]
    );
}

#[test]
fn human_name_pool_continues_without_reusing_names() {
    let r = Radio::new();
    let names: Vec<String> = (0..27)
        .map(|index| {
            r.ok(&["register", "--client-id", &format!("pool-session-{index}")])
                .trim()
                .to_string()
        })
        .collect();
    assert_eq!(names[0], "Alice");
    assert_eq!(names[25], "Zachary");
    assert_eq!(names[26], "Alice-2");
}

#[test]
fn broadcast_notifies_registered_agents_before_their_first_message() {
    let r = Radio::new();
    r.ok(&["register", "--client-id", "session-alice"]);
    r.ok(&["register", "--client-id", "session-bob"]);

    let mut alice = r.cmd();
    alice.env("AGENT_RADIO_CLIENT_ID", "session-alice");
    let sent = alice
        .args([
            "send",
            "--to",
            "all",
            "--kind",
            "FYI",
            "--body",
            "Standup now",
            "--branch",
            "",
        ])
        .output()
        .unwrap();
    assert!(sent.status.success());

    let mut bob = r.cmd();
    bob.env("AGENT_RADIO_CLIENT_ID", "session-bob");
    let status = bob.args(["status"]).output().unwrap();
    assert!(status.status.success());
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["agent"], "Bob");
    assert_eq!(status["unread"], 1);
    assert_eq!(status["flag"], true);
}

#[test]
fn rename_is_an_alias_over_stable_identity_and_pending_messages() {
    let r = Radio::new();
    r.ok(&[
        "register",
        "--client-id",
        "session-alice",
        "--provider",
        "claude",
    ]);
    r.ok(&["register", "--client-id", "session-bob"]);
    send(&r, "Bob", "Alice", "before rename");

    let mut alice = r.cmd();
    alice.env("AGENT_RADIO_CLIENT_ID", "session-alice");
    let renamed = alice
        .args(["rename", "--name", "Maverick"])
        .output()
        .unwrap();
    assert!(renamed.status.success());
    assert_eq!(
        String::from_utf8(renamed.stdout).unwrap().trim(),
        "Maverick\tAlice"
    );

    let mut alice = r.cmd();
    alice.env("AGENT_RADIO_CLIENT_ID", "session-alice");
    let inbox = alice.args(["inbox"]).output().unwrap();
    assert!(inbox.status.success());
    let inbox = String::from_utf8(inbox.stdout).unwrap();
    assert!(inbox.contains("Bob -> Maverick"));
    assert!(inbox.contains("before rename"));
    let reply = r.ok(&["ack", "1", "--as", "Maverick", "--body", "got it"]);
    assert!(reply.contains("-> Bob"));

    send(&r, "Bob", "Maverick", "after rename");
    let messages = r.messages();
    assert_eq!(messages[0]["to"], "Alice");
    assert_eq!(messages[1]["from"], "Alice");
    assert_eq!(messages[1]["to"], "Bob");
    assert_eq!(messages[2]["to"], "Alice");
    let history = r.ok(&["history", "--with", "Maverick"]);
    assert!(history.contains("before rename"));
    assert!(history.contains("after rename"));
    assert!(history.contains("Bob -> Maverick"));

    let mut status = r.cmd();
    status.env("AGENT_RADIO_CLIENT_ID", "session-alice");
    let status = status.args(["status"]).output().unwrap();
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["agent"], "Alice");
    assert_eq!(status["display_name"], "Maverick");
    assert!(r
        .ok(&["team"])
        .lines()
        .next()
        .unwrap()
        .starts_with("Maverick\tclaude\t"));
}

#[test]
fn reset_keeps_previous_alias_routable_and_unavailable_to_others() {
    let r = Radio::new();
    r.ok(&["register", "--client-id", "session-alice"]);
    r.ok(&["register", "--client-id", "session-bob"]);
    r.ok(&[
        "rename",
        "--client-id",
        "session-alice",
        "--name",
        "Maverick",
    ]);
    r.ok(&["rename", "--client-id", "session-alice", "--name", "Atlas"]);
    r.ok(&["rename", "--client-id", "session-alice", "--reset"]);

    send(&r, "Bob", "Maverick", "old alias");
    assert_eq!(r.messages()[0]["to"], "Alice");
    r.fails(&["rename", "--client-id", "session-bob", "--name", "maverick"]);
}

#[test]
fn rename_rejects_reserved_legacy_and_concurrent_collisions() {
    let r = Radio::new();
    r.ok(&["register", "--client-id", "session-alice"]);
    r.ok(&["register", "--client-id", "session-bob"]);
    send(&r, "legacy", "Alice", "hello");
    for name in ["all", "Bob", "Alice-2", "legacy"] {
        r.fails(&["rename", "--client-id", "session-alice", "--name", name]);
    }

    let store = r.dir.path().to_path_buf();
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = ["session-alice", "session-bob"]
        .into_iter()
        .map(|client_id| {
            let store = store.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                Command::new(env!("CARGO_BIN_EXE_agent-radio"))
                    .env("AGENT_RADIO_DIR", store)
                    .args(["rename", "--client-id", client_id, "--name", "Maverick"])
                    .output()
                    .unwrap()
                    .status
                    .success()
            })
        })
        .collect();
    let successes = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .filter(|success| *success)
        .count();
    assert_eq!(successes, 1);
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
    send(&r, "a", "b", "left\u{202e}right\u{200b}end");
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
    assert!(rendered.contains("leftrightend"));
    assert!(!rendered
        .chars()
        .any(|character| matches!(character, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')));
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

#[test]
fn manifest_require_env_done_without_manifest_fails() {
    let r = Radio::new();
    let wd = r.work_dir();
    fs::write(wd.join("workfile.txt"), b"dirty").unwrap();

    let mut c = r.cmd_in(&wd);
    c.env("AGENT_RADIO_REQUIRE_MANIFEST", "1");
    let out = c
        .args([
            "send",
            "--from",
            "bot",
            "--to",
            "human",
            "--kind",
            "DONE",
            "--body",
            "no manifest",
        ])
        .output()
        .unwrap();

    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("DONE sin manifiesto"));
    assert!(r.messages().is_empty());
}

#[test]
fn manifest_require_env_done_with_no_manifest_ok() {
    let r = Radio::new();
    let wd = r.work_dir();
    fs::write(wd.join("workfile.txt"), b"dirty").unwrap();

    let mut c = r.cmd_in(&wd);
    c.env("AGENT_RADIO_REQUIRE_MANIFEST", "1");
    let out = c
        .args([
            "send",
            "--from",
            "bot",
            "--to",
            "human",
            "--kind",
            "DONE",
            "--body",
            "explicit",
            "--no-manifest",
        ])
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let msgs = r.messages();
    assert_eq!(msgs.len(), 1);
    assert!(msgs[0].get("manifest").is_none());
}

#[test]
fn manifest_require_env_done_with_manifest_ok() {
    let r = Radio::new();
    let wd = r.work_dir();
    fs::write(wd.join("workfile.txt"), b"dirty").unwrap();

    let mut c = r.cmd_in(&wd);
    c.env("AGENT_RADIO_REQUIRE_MANIFEST", "1");
    let out = c
        .args([
            "send",
            "--from",
            "bot",
            "--to",
            "human",
            "--kind",
            "DONE",
            "--body",
            "with manifest",
            "--manifest",
            "workfile.txt",
        ])
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let msgs = r.messages();
    assert_eq!(msgs.len(), 1);
    assert!(msgs[0].get("manifest").is_some());
}

#[test]
fn manifest_require_env_done_reply_without_manifest_fails() {
    let r = Radio::new();
    let wd = r.work_dir();
    r.ok_in(
        &[
            "send", "--from", "bot", "--to", "human", "--kind", "ASK", "--body", "question",
        ],
        &wd,
    );
    r.ok_in(&["inbox", "--as", "human"], &wd);

    let mut c = r.cmd_in(&wd);
    c.env("AGENT_RADIO_REQUIRE_MANIFEST", "1");
    let out = c
        .args(["done", "1", "--as", "human", "--body", "reply"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("DONE sin manifiesto"));

    let mut c = r.cmd_in(&wd);
    c.env("AGENT_RADIO_REQUIRE_MANIFEST", "1");
    let out = c
        .args([
            "done",
            "1",
            "--as",
            "human",
            "--body",
            "reply",
            "--no-manifest",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn manifest_require_env_not_set_still_works() {
    let r = Radio::new();
    let wd = r.work_dir();
    let out = r.run_in(
        &[
            "send",
            "--from",
            "bot",
            "--to",
            "human",
            "--kind",
            "DONE",
            "--body",
            "no manifest no flag",
        ],
        &wd,
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn manifest_auto_embeds_dirty_files() {
    let r = Radio::new();
    let wd = r.work_dir();
    fs::write(wd.join("a.txt"), b"a").unwrap();
    fs::write(wd.join("b.txt"), b"b").unwrap();

    r.ok_in(
        &[
            "send",
            "--from",
            "bot",
            "--to",
            "human",
            "--body",
            "auto",
            "--manifest-auto",
        ],
        &wd,
    );

    let msgs = r.messages();
    let files = msgs[0]["manifest"]["files"].as_object().unwrap();
    assert!(files.contains_key("a.txt"));
    assert!(files.contains_key("b.txt"));
}

#[test]
fn manifest_auto_and_manifest_conflict() {
    let r = Radio::new();
    assert!(!r
        .run(&[
            "send",
            "--from",
            "bot",
            "--to",
            "human",
            "--body",
            "both",
            "--manifest-auto",
            "--manifest",
            "x.txt",
        ])
        .status
        .success());
    assert!(!r
        .run(&[
            "send",
            "--from",
            "bot",
            "--to",
            "human",
            "--body",
            "mixed",
            "--manifest-auto",
            "--no-manifest",
        ])
        .status
        .success());
    assert!(!r
        .run(&[
            "send",
            "--from",
            "bot",
            "--to",
            "human",
            "--body",
            "mixed2",
            "--manifest",
            "x.txt",
            "--no-manifest",
        ])
        .status
        .success());
}

#[test]
fn manifest_verify_strict_ignore_orphans() {
    let r = Radio::new();
    let wd = r.work_dir();
    fs::write(wd.join("claimed.txt"), b"claimed").unwrap();
    r.ok_in(
        &[
            "send",
            "--from",
            "bot",
            "--to",
            "human",
            "--body",
            "claim",
            "--manifest",
            "claimed.txt",
        ],
        &wd,
    );
    fs::write(wd.join("orphan.txt"), b"orphan").unwrap();
    fs::write(wd.join("build.lock"), b"lock").unwrap();
    r.ok_in(&["history", "--as", "human"], &wd);

    let out = r.run_in(
        &[
            "manifest", "verify", "1", "--as", "human", "--strict", "--ignore", "*.lock",
        ],
        &wd,
    );
    assert!(!out.status.success());
    assert_eq!(out.status.code(), Some(3));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("HUERFANO"));
    assert!(stderr.contains("orphan.txt"));
    assert!(!stderr.contains("build.lock"));

    let out = r.run_in(
        &[
            "manifest", "verify", "1", "--as", "human", "--strict", "--ignore", "*.lock",
            "--ignore", "orphan*",
        ],
        &wd,
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn manifest_map_strict_ignore_orphans() {
    let r = Radio::new();
    let wd = r.work_dir();
    fs::write(wd.join("claimed.txt"), b"claimed").unwrap();
    r.ok_in(
        &[
            "send",
            "--from",
            "bot",
            "--to",
            "human",
            "--body",
            "claim",
            "--manifest",
            "claimed.txt",
        ],
        &wd,
    );
    fs::write(wd.join("orphan.txt"), b"orphan").unwrap();
    fs::write(wd.join("build.lock"), b"lock").unwrap();

    let out = r.run_in(&["manifest", "map", "--strict", "--ignore", "*.lock"], &wd);
    assert!(!out.status.success());
    assert_eq!(out.status.code(), Some(3));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("HUERFANO"));
    assert!(stderr.contains("orphan.txt"));
    assert!(!stderr.contains("build.lock"));

    let out = r.run_in(&["manifest", "map", "--strict", "--ignore", "*"], &wd);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!String::from_utf8_lossy(&out.stderr).contains("HUERFANO"));
}

#[test]
fn manifest_digest_corrupt_detected() {
    let r = Radio::new();
    let wd = r.work_dir();
    fs::write(wd.join("digest.txt"), b"stable").unwrap();
    r.ok_in(
        &[
            "send",
            "--from",
            "bot",
            "--to",
            "human",
            "--body",
            "digest",
            "--manifest",
            "digest.txt",
        ],
        &wd,
    );
    r.ok_in(&["history", "--as", "human"], &wd);

    let path = r.dir.path().join("messages.jsonl");
    let mut msg = r.messages().remove(0);
    msg["manifest"]["digest"] = Value::String(
        "0000111122223333444455556666777788889999aaaabbbbccccddddeeeeffff0000".into(),
    );
    fs::write(path, format!("{}\n", serde_json::to_string(&msg).unwrap())).unwrap();

    let out = r.run_in(&["manifest", "verify", "1", "--as", "human"], &wd);
    assert!(!out.status.success());
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("DIGEST corrupto"));
}

#[test]
fn manifest_digest_consistent_but_file_mismatch() {
    let r = Radio::new();
    let wd = r.work_dir();
    fs::write(wd.join("mismatch.txt"), b"original").unwrap();
    r.ok_in(
        &[
            "send",
            "--from",
            "bot",
            "--to",
            "human",
            "--body",
            "digest",
            "--manifest",
            "mismatch.txt",
        ],
        &wd,
    );
    fs::write(wd.join("mismatch.txt"), b"modified").unwrap();
    r.ok_in(&["history", "--as", "human"], &wd);

    let out = r.run_in(&["manifest", "verify", "1", "--as", "human"], &wd);
    assert!(!out.status.success());
    assert_eq!(out.status.code(), Some(2));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stdout.contains("MISMATCH"));
    assert!(stdout.contains("mismatch.txt"));
    assert!(!stdout.contains("DIGEST"));
    assert!(!stderr.contains("DIGEST"));
}

// ------------------------------------------------ security regressions --

#[cfg(unix)]
#[test]
fn store_and_sidecars_are_private_by_default() {
    let r = Radio::new();
    r.ok(&[
        "register",
        "--client-id",
        "security-session",
        "--provider",
        "test",
    ]);
    send(&r, "Alice", "Bob", "private");

    for dir in ["", "seen", "views", "notify"] {
        let mode = fs::metadata(r.dir.path().join(dir))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700, "{dir:?} must be private");
    }
    for file in ["lock", "agents.json", "messages.jsonl", "notify/Bob.flag"] {
        let mode = fs::metadata(r.dir.path().join(file))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "{file} must be private");
    }
}

#[cfg(unix)]
#[test]
fn poisoned_message_identity_cannot_write_outside_notify_dir() {
    let r = Radio::new();
    let victim_base = r.dir.path().join("victim");
    let victim_flag = r.dir.path().join("victim.flag");
    fs::write(&victim_flag, "SAFE").unwrap();
    let poisoned = serde_json::json!({
        "id": "poison",
        "ts": "2026-01-01T00:00:00.000000Z",
        "from": victim_base.to_string_lossy(),
        "to": "nobody",
        "kind": "FYI",
        "body": "poison"
    });
    fs::write(r.dir.path().join("messages.jsonl"), format!("{poisoned}\n")).unwrap();

    send(&r, "Alice", "all", "broadcast");
    assert_eq!(fs::read_to_string(victim_flag).unwrap(), "SAFE");
}

#[cfg(unix)]
#[test]
fn predictable_atomic_temp_symlink_cannot_overwrite_victim() {
    let r = Radio::new();
    let victim = r.dir.path().join("victim");
    fs::write(&victim, "SAFE").unwrap();
    symlink(&victim, r.dir.path().join("agents.json.tmp")).unwrap();

    r.ok(&["register", "--client-id", "secure-session"]);

    assert_eq!(fs::read_to_string(victim).unwrap(), "SAFE");
    assert!(!fs::symlink_metadata(r.dir.path().join("agents.json"))
        .unwrap()
        .file_type()
        .is_symlink());
}

#[test]
fn manifest_verify_rejects_traversal_without_hashing_external_file() {
    let r = Radio::new();
    let wd = r.work_dir();
    let secret = r.dir.path().join("secret.txt");
    fs::write(&secret, b"outside secret").unwrap();
    let reported = "0".repeat(64);
    let claim = format!("../secret.txt:{reported}");
    let digest = format!("{:x}", Sha256::digest(claim.as_bytes()));
    let message = serde_json::json!({
        "id": "evil",
        "ts": "2026-01-01T00:00:00.000000Z",
        "from": "Mallory",
        "to": "Alice",
        "kind": "DONE",
        "body": "evil",
        "task": "evil",
        "manifest": {
            "files": { "../secret.txt": reported },
            "digest": digest
        }
    });
    fs::write(r.dir.path().join("messages.jsonl"), format!("{message}\n")).unwrap();

    let out = r.run_in(&["manifest", "verify", "--task", "evil"], &wd);
    assert!(!out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let secret_hash = format!("{:x}", Sha256::digest(b"outside secret"));
    assert!(!stdout.contains(&secret_hash[..12]));
    assert!(stderr.contains("unsafe manifest path"));
}

#[test]
fn malformed_manifest_map_fails_cleanly_without_panicking() {
    let r = Radio::new();
    let wd = r.work_dir();
    let message = serde_json::json!({
        "id": "bad",
        "ts": "2026-01-01T00:00:00.000000Z",
        "from": "Mallory",
        "to": "Alice",
        "kind": "DONE",
        "body": "bad",
        "task": "bad",
        "manifest": null
    });
    fs::write(r.dir.path().join("messages.jsonl"), format!("{message}\n")).unwrap();

    let out = r.run_in(&["manifest", "map"], &wd);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stdout).contains("CORRUPTO"));
    assert!(!String::from_utf8_lossy(&out.stderr).contains("panicked at"));
}

#[test]
fn duplicate_message_ids_are_rejected_as_ambiguous() {
    let r = Radio::new();
    let first = serde_json::json!({
        "id": "duplicate",
        "ts": "2026-01-01T00:00:00.000000Z",
        "from": "Alice",
        "to": "Bob",
        "kind": "ASK",
        "body": "first"
    });
    let second = serde_json::json!({
        "id": "duplicate",
        "ts": "2026-01-01T00:00:01.000000Z",
        "from": "Charlie",
        "to": "Bob",
        "kind": "FYI",
        "body": "second"
    });
    fs::write(
        r.dir.path().join("messages.jsonl"),
        format!("{first}\n{second}\n"),
    )
    .unwrap();

    let out = r.fails(&["history"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("duplicate message id"));
}

#[test]
fn oversized_stdin_body_is_rejected_before_append() {
    let r = Radio::new();
    let body = "x".repeat(256 * 1024 + 1);
    let out = run_with_stdin(
        &r,
        &[
            "send", "--from", "Alice", "--to", "Bob", "--body", "-", "--branch", "",
        ],
        &body,
    );

    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("body exceeds"));
    assert!(r.messages().is_empty());
}
