//! Local-only agent radio.
//!
//! Persistent, file-backed messages for multiple local coding agents sharing
//! the same machine. By default state lives under
//! `<git-root>/.git/.agent-radio/` (never committed, never pushed); set
//! `AGENT_RADIO_DIR` to use any directory and run outside git worktrees
//! entirely.
//!
//! Environment:
//!   AGENT_RADIO_DIR    store directory (default: <git-root>/.git/.agent-radio)
//!   AGENT_RADIO_AGENT  default identity for --as/--from
//!
//! The store format (JSONL messages + `seen`/`views`/`notify` sidecars) is the
//! compatibility contract: any implementation that preserves it interoperates.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{exit, Command};
use std::sync::LazyLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand};
use regex::Regex;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use time::format_description::BorrowedFormatItem;
use time::macros::format_description;
use time::OffsetDateTime;

const TS_FORMAT: &[BorrowedFormatItem<'_>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:6]Z");

const KINDS: [&str; 10] = [
    "ACK",
    "ASK",
    "BLOCKED",
    "DECLINE",
    "DONE",
    "FAILURE",
    "FYI",
    "HANDOFF",
    "REVIEW_REQUEST",
    "RISK",
];

static NAME_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9._-]+$").unwrap());

static SECRET_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"\bgh[pousr]_[A-Za-z0-9]{20,}\b",
        r"\bgithub_pat_[A-Za-z0-9_]{40,}\b",
        r"\bsk-ant-[A-Za-z0-9_-]{40,}\b",
        r"\bsk-(?:proj-)?[A-Za-z0-9_-]{40,}\b",
        r"\bAIza[0-9A-Za-z_-]{20,}\b",
        r"\b[A-Za-z][A-Za-z0-9+.-]*://[^\s:/@]+:[^\s/@]+@",
        r#"(?i)(api[_-]?key|secret|token|password|passwd)\s*[:=]\s*['"]?[A-Za-z0-9_./+=-]{24,}"#,
    ]
    .iter()
    .map(|p| Regex::new(p).unwrap())
    .collect()
});

fn die(msg: &str) -> ! {
    eprintln!("{msg}");
    exit(1);
}

// ---------------------------------------------------------------- store --

struct Store {
    root: PathBuf,
    messages: PathBuf,
    lock: PathBuf,
    seen_dir: PathBuf,
    views_dir: PathBuf,
    notify_dir: PathBuf,
}

fn git_root() -> PathBuf {
    let out = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output();
    match out {
        Ok(o) if o.status.success() => {
            PathBuf::from(String::from_utf8_lossy(&o.stdout).trim().to_string())
        }
        _ => die("agent-radio: run inside a git worktree (or set AGENT_RADIO_DIR)"),
    }
}

fn current_branch() -> Option<String> {
    let out = Command::new("git")
        .args(["branch", "--show-current"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if branch.is_empty() {
        None
    } else {
        Some(branch)
    }
}

fn store_root() -> PathBuf {
    if let Ok(dir) = std::env::var("AGENT_RADIO_DIR") {
        if !dir.is_empty() {
            if let Some(rest) = dir.strip_prefix("~/") {
                if let Some(home) = std::env::var_os("HOME") {
                    return PathBuf::from(home).join(rest);
                }
            }
            return PathBuf::from(dir);
        }
    }
    git_root().join(".git").join(".agent-radio")
}

fn store() -> Store {
    let root = store_root();
    Store {
        messages: root.join("messages.jsonl"),
        lock: root.join("lock"),
        seen_dir: root.join("seen"),
        views_dir: root.join("views"),
        notify_dir: root.join("notify"),
        root,
    }
}

/// Exclusive advisory lock on the store; released when the guard drops.
struct LockGuard(fs::File);

fn locked(s: &Store) -> LockGuard {
    fs::create_dir_all(&s.root).unwrap_or_else(|e| die(&format!("agent-radio: {e}")));
    // read+write (not append-only): Windows' LockFileEx rejects handles
    // without real read/write access — the Python impl's "a+" for the
    // same reason. Never truncated, never written; it exists to be locked.
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&s.lock)
        .unwrap_or_else(|e| die(&format!("agent-radio: {e}")));
    file.lock()
        .unwrap_or_else(|e| die(&format!("agent-radio: {e}")));
    LockGuard(file)
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

// ----------------------------------------------------------- validation --

fn validate_name(name: &str) -> &str {
    if !NAME_RE.is_match(name) {
        die(&format!(
            "agent-radio: invalid agent name {name:?}; use letters, digits, '.', '_' or '-'"
        ));
    }
    name
}

fn require_no_secret(text: &str) {
    if SECRET_PATTERNS.iter().any(|p| p.is_match(text)) {
        die(
            "agent-radio: message looks like it contains a secret. Do not send tokens, \
             credentials, connection strings, or raw env output.",
        );
    }
}

fn agent_from_env(explicit: Option<&str>) -> String {
    let name = explicit.map(str::to_string).or_else(|| {
        std::env::var("AGENT_RADIO_AGENT")
            .ok()
            .filter(|v| !v.is_empty())
    });
    match name {
        Some(n) => {
            validate_name(&n);
            n
        }
        None => die("agent-radio: pass --as/--from or set AGENT_RADIO_AGENT"),
    }
}

// ------------------------------------------------------------- messages --

fn utc_now() -> String {
    OffsetDateTime::now_utc()
        .format(TS_FORMAT)
        .unwrap_or_else(|e| die(&format!("agent-radio: {e}")))
}

fn gen_id(ts: &str, sender: &str, to: &str, body: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seed = format!("{ts}\0{sender}\0{to}\0{body}\0{nanos}");
    let digest = Sha256::digest(seed.as_bytes());
    digest
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()[..16]
        .to_string()
}

fn sha256_bytes(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn sha256_file(path: &Path) -> Option<String> {
    let data = fs::read(path).ok()?;
    Some(sha256_bytes(&data))
}

/// Compute manifest digest per contract: lines "<path>:<hash>" or "<path>:(deleted)" sorted by path.
fn compute_manifest_digest(files: &BTreeMap<String, Option<String>>) -> String {
    let mut lines = Vec::new();
    for (path, hash) in files {
        let line = match hash {
            Some(h) => format!("{path}:{h}"),
            None => format!("{path}:(deleted)"),
        };
        lines.push(line);
    }
    let input = lines.join("\n");
    sha256_bytes(input.as_bytes())
}

/// Returns file paths (relative to git root) from `git status --porcelain -uall`.
/// For renames ("R N old -> new"), takes the new path after the LAST " -> ".
fn git_status_files() -> Vec<String> {
    let out = Command::new("git")
        .args(["status", "--porcelain", "-uall"])
        .output()
        .unwrap_or_else(|e| die(&format!("agent-radio: {e}")));
    if !out.status.success() {
        return Vec::new();
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut files = Vec::new();
    for line in stdout.lines() {
        if line.len() < 4 {
            continue;
        }
        let rest = &line[3..];
        if let Some(pos) = rest.rfind(" -> ") {
            files.push(rest[pos + 4..].to_string());
        } else {
            files.push(rest.to_string());
        }
    }
    files
}

/// Hash paths relative to cwd, store as relative to git root.
/// Returns Map suitable for inserting as "manifest" in a message.
fn make_manifest(paths: &[String]) -> Map<String, Value> {
    let git_root = git_root();
    let mut files: BTreeMap<String, Option<String>> = BTreeMap::new();
    for path_str in paths {
        let p = Path::new(path_str);
        let abs = if p.is_absolute() {
            p.to_path_buf()
        } else {
            std::env::current_dir().unwrap_or_default().join(p)
        };
        let rel = abs
            .strip_prefix(&git_root)
            .unwrap_or_else(|_| {
                die(&format!(
                    "agent-radio: path {path_str} is outside git worktree"
                ))
            })
            .to_string_lossy()
            .to_string();
        let hash = sha256_file(&abs);
        files.insert(rel, hash);
    }
    let digest = compute_manifest_digest(&files);
    let mut m = Map::new();
    m.insert("files".into(), json!(files));
    m.insert("digest".into(), json!(digest));
    m
}

fn manifest_option_count(manifest: &[String], no_manifest: bool, manifest_auto: bool) -> usize {
    usize::from(!manifest.is_empty()) + usize::from(no_manifest) + usize::from(manifest_auto)
}

fn validate_manifest_options(manifest: &[String], no_manifest: bool, manifest_auto: bool) {
    if manifest_option_count(manifest, no_manifest, manifest_auto) > 1 {
        die("agent-radio: --manifest, --manifest-auto and --no-manifest are mutually exclusive");
    }
}

fn enforce_done_manifest_policy(
    kind: &str,
    manifest: &[String],
    no_manifest: bool,
    manifest_auto: bool,
) {
    let required = std::env::var("AGENT_RADIO_REQUIRE_MANIFEST").is_ok_and(|v| v == "1");
    let has_option = manifest_option_count(manifest, no_manifest, manifest_auto) > 0;
    if required && kind.to_uppercase() == "DONE" && !has_option {
        die(
            "agent-radio: DONE sin manifiesto: adjuntá --manifest <archivos> o declarà \
             --no-manifest si la tarea no editó archivos",
        );
    }
}

fn insert_requested_manifest(
    msg: &mut Map<String, Value>,
    manifest: &[String],
    no_manifest: bool,
    manifest_auto: bool,
) {
    if manifest_auto {
        let dirty_paths = git_status_files();
        if !dirty_paths.is_empty() {
            let m = make_manifest(&dirty_paths);
            msg.insert("manifest".into(), json!(m));
        }
    } else if no_manifest {
        // Explicit declaration for enforcement only; it is not persisted.
    } else if !manifest.is_empty() {
        let m = make_manifest(manifest);
        msg.insert("manifest".into(), json!(m));
    }
}

/// Simple glob matcher: `*` matches any chars except `/`, `**` matches any chars including `/`.
/// Other chars match literally. Split on `/` and match segment by segment.
fn glob_match(pattern: &str, path: &str) -> bool {
    let pat: Vec<&str> = pattern.split('/').collect();
    let segs: Vec<&str> = path.split('/').collect();
    glob_match_segs(&pat, &segs)
}

fn glob_match_segs(pat: &[&str], segs: &[&str]) -> bool {
    match (pat.is_empty(), segs.is_empty()) {
        (true, true) => return true,
        (true, false) => return false,
        (false, true) => return pat.iter().all(|p| *p == "**"),
        _ => {}
    }
    if pat[0] == "**" {
        // ** matches zero or more segments
        if glob_match_segs(&pat[1..], segs) {
            return true;
        }
        for i in 1..=segs.len() {
            if glob_match_segs(&pat[1..], &segs[i..]) {
                return true;
            }
        }
        return false;
    }
    if !segment_match(pat[0], segs[0]) {
        return false;
    }
    glob_match_segs(&pat[1..], &segs[1..])
}

fn segment_match(pattern: &str, segment: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let s: Vec<char> = segment.chars().collect();
    seg_match_chars(&p, &s, 0, 0)
}

fn seg_match_chars(p: &[char], s: &[char], pi: usize, si: usize) -> bool {
    if pi == p.len() {
        return si == s.len();
    }
    if p[pi] == '*' {
        if si >= s.len() {
            return seg_match_chars(p, s, pi + 1, si);
        }
        seg_match_chars(p, s, pi + 1, si) || seg_match_chars(p, s, pi, si + 1)
    } else {
        if si >= s.len() || p[pi] != s[si] {
            return false;
        }
        seg_match_chars(p, s, pi + 1, si + 1)
    }
}

fn load_messages(s: &Store) -> Vec<Map<String, Value>> {
    let Ok(text) = fs::read_to_string(&s.messages) else {
        return Vec::new();
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter_map(|v| match v {
            Value::Object(m) if m.get("id").is_some_and(Value::is_string) => Some(m),
            _ => None,
        })
        .collect()
}

fn append_message(s: &Store, msg: &Map<String, Value>) {
    fs::create_dir_all(&s.root).unwrap_or_else(|e| die(&format!("agent-radio: {e}")));
    let mut fh = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&s.messages)
        .unwrap_or_else(|e| die(&format!("agent-radio: {e}")));
    let line = serde_json::to_string(msg).unwrap_or_else(|e| die(&format!("agent-radio: {e}")));
    writeln!(fh, "{line}").unwrap_or_else(|e| die(&format!("agent-radio: {e}")));
}

fn str_field<'a>(msg: &'a Map<String, Value>, key: &str) -> &'a str {
    msg.get(key).and_then(Value::as_str).unwrap_or("")
}

// --------------------------------------------------------------- notify --

fn notify_path(s: &Store, agent: &str) -> PathBuf {
    s.notify_dir.join(format!("{agent}.flag"))
}

fn known_agents(messages: &[Map<String, Value>]) -> BTreeSet<String> {
    let mut agents = BTreeSet::new();
    for msg in messages {
        let sender = str_field(msg, "from");
        if !sender.is_empty() {
            agents.insert(sender.to_string());
        }
        let to = str_field(msg, "to");
        if !to.is_empty() && to != "all" {
            agents.insert(to.to_string());
        }
    }
    agents
}

fn set_notify_flags(s: &Store, msg: &Map<String, Value>, messages: &[Map<String, Value>]) {
    fs::create_dir_all(&s.notify_dir).unwrap_or_else(|e| die(&format!("agent-radio: {e}")));
    let to = str_field(msg, "to");
    let recipients: BTreeSet<String> = if to == "all" {
        let mut r = known_agents(messages);
        r.remove(str_field(msg, "from"));
        r
    } else if !to.is_empty() {
        BTreeSet::from([to.to_string()])
    } else {
        BTreeSet::new()
    };
    for agent in recipients {
        let _ = fs::write(notify_path(s, &agent), str_field(msg, "id"));
    }
}

fn clear_notify_if_caught_up(s: &Store, agent: &str, unread: &[Map<String, Value>]) {
    if unread.is_empty() {
        let _ = fs::remove_file(notify_path(s, agent));
    }
}

// ----------------------------------------------------------- seen/views --

fn load_seen(s: &Store, agent: &str) -> BTreeSet<String> {
    let path = s.seen_dir.join(format!("{agent}.json"));
    let Ok(text) = fs::read_to_string(path) else {
        return BTreeSet::new();
    };
    serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|v| {
            v.get("seen").and_then(Value::as_array).map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
        })
        .unwrap_or_default()
}

fn write_json_atomic(dir: &PathBuf, name: &str, value: &Value) {
    fs::create_dir_all(dir).unwrap_or_else(|e| die(&format!("agent-radio: {e}")));
    let path = dir.join(name);
    let tmp = dir.join(format!("{name}.tmp"));
    let text =
        serde_json::to_string_pretty(value).unwrap_or_else(|e| die(&format!("agent-radio: {e}")));
    fs::write(&tmp, text).unwrap_or_else(|e| die(&format!("agent-radio: {e}")));
    fs::rename(&tmp, &path).unwrap_or_else(|e| die(&format!("agent-radio: {e}")));
}

fn save_seen(s: &Store, agent: &str, seen: &BTreeSet<String>) {
    let sorted: Vec<&String> = seen.iter().collect();
    write_json_atomic(
        &s.seen_dir,
        &format!("{agent}.json"),
        &json!({ "seen": sorted }),
    );
}

fn save_view(s: &Store, agent: &str, ids: &[String]) {
    write_json_atomic(
        &s.views_dir,
        &format!("{agent}.json"),
        &json!({ "ids": ids }),
    );
}

fn load_view(s: &Store, agent: &str) -> Vec<String> {
    let path = s.views_dir.join(format!("{agent}.json"));
    let Ok(text) = fs::read_to_string(path) else {
        die("agent-radio: no last view; run inbox or history first");
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        die("agent-radio: last view is corrupt; run inbox or history again");
    };
    value
        .get("ids")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn addressed_to(msg: &Map<String, Value>, agent: &str) -> bool {
    let to = str_field(msg, "to");
    to == agent || (to == "all" && str_field(msg, "from") != agent)
}

fn unread_for(s: &Store, agent: &str) -> Vec<Map<String, Value>> {
    let seen = load_seen(s, agent);
    load_messages(s)
        .into_iter()
        .filter(|m| addressed_to(m, agent) && !seen.contains(str_field(m, "id")))
        .collect()
}

// -------------------------------------------------------------- render --

/// Terminal-injection guard: rendered messages must never carry control
/// characters (CSI/OSC escapes, backspace forgery, C1 controls) into the
/// reader's terminal. \t/\r/\n become spaces; everything else control-ish is
/// dropped (`char::is_control` = C0 + DEL + C1), then whitespace collapses.
fn sanitize(text: &str) -> String {
    let spaced: String = text
        .chars()
        .map(|c| {
            if matches!(c, '\t' | '\r' | '\n') {
                ' '
            } else {
                c
            }
        })
        .filter(|c| !c.is_control())
        .collect();
    spaced.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn short(text: &str, limit: usize) -> String {
    let s = sanitize(text);
    if s.chars().count() <= limit {
        return s;
    }
    let mut out: String = s.chars().take(limit - 1).collect();
    out.push('…');
    out
}

/// render() is the trust boundary: any process can append to the JSONL, so
/// every displayed field goes through sanitize(), not just the body.
fn render(messages: &[Map<String, Value>]) -> Vec<String> {
    let mut lines = Vec::new();
    for (idx, msg) in messages.iter().enumerate() {
        let opt = |key: &str, prefix: &str| -> String {
            let v = str_field(msg, key);
            if v.is_empty() {
                String::new()
            } else {
                format!(" · {prefix}{}", sanitize(v))
            }
        };
        lines.push(format!(
            "{:>2}. {} {} -> {} {}{}{}{} #{}",
            idx + 1,
            sanitize(str_field(msg, "ts")),
            sanitize(str_field(msg, "from")),
            sanitize(str_field(msg, "to")),
            sanitize(str_field(msg, "kind")),
            opt("priority", ""),
            opt("branch", ""),
            opt("reply_to", "re "),
            sanitize(str_field(msg, "id")),
        ));
        let focus: Vec<String> = msg
            .get("focus")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .map(|v| sanitize(v.as_str().unwrap_or_default()))
                    .collect()
            })
            .unwrap_or_default();
        if !focus.is_empty() {
            lines.push(format!("    focus: {}", focus.join(", ")));
        }
        let risk = str_field(msg, "risk");
        if !risk.is_empty() {
            lines.push(format!("    risk : {}", short(risk, 180)));
        }
        lines.push(format!("    {}", short(str_field(msg, "body"), 260)));
        if msg.contains_key("manifest") {
            if let Some(m) = msg["manifest"].as_object() {
                let n_files = m
                    .get("files")
                    .and_then(Value::as_object)
                    .map(|o| o.len())
                    .unwrap_or(0);
                let digest = m.get("digest").and_then(Value::as_str).unwrap_or("");
                let short_digest = if digest.len() > 8 {
                    &digest[..8]
                } else {
                    digest
                };
                lines.push(format!("    [manifest {n_files} files @{short_digest}]"));
            }
        }
    }
    lines
}

fn print_rendered(messages: &[Map<String, Value>]) {
    println!("{}", render(messages).join("\n"));
}

// ------------------------------------------------------------- compose --

#[allow(clippy::too_many_arguments)]
fn make_message(
    sender: &str,
    to: &str,
    kind: &str,
    body: &str,
    branch: Option<&str>,
    focus: &[String],
    risk: Option<&str>,
    priority: Option<&str>,
    reply_to: Option<&str>,
    thread_id: Option<&str>,
) -> Map<String, Value> {
    validate_name(sender);
    validate_name(to);
    let kind = kind.to_uppercase();
    if !KINDS.contains(&kind.as_str()) {
        die(&format!(
            "agent-radio: invalid kind {kind}; use one of {}",
            KINDS.join(", ")
        ));
    }
    let body = body.trim();
    if body.is_empty() {
        die("agent-radio: empty body");
    }
    let secret_scan = format!("{body}\n{}\n{}", risk.unwrap_or(""), focus.join("\n"));
    require_no_secret(&secret_scan);

    let ts = utc_now();
    let id = gen_id(&ts, sender, to, body);
    let mut msg = Map::new();
    msg.insert("version".into(), json!(1));
    msg.insert("id".into(), json!(id));
    msg.insert("ts".into(), json!(ts));
    msg.insert("from".into(), json!(sender));
    msg.insert("to".into(), json!(to));
    msg.insert("kind".into(), json!(kind));
    msg.insert("body".into(), json!(body));
    if let Some(b) = branch.filter(|b| !b.is_empty()) {
        msg.insert("branch".into(), json!(b));
    }
    if !focus.is_empty() {
        msg.insert("focus".into(), json!(focus));
    }
    if let Some(r) = risk.filter(|r| !r.is_empty()) {
        msg.insert("risk".into(), json!(r));
    }
    if let Some(p) = priority.filter(|p| !p.is_empty()) {
        msg.insert("priority".into(), json!(p.to_lowercase()));
    }
    if let Some(r) = reply_to {
        msg.insert("reply_to".into(), json!(r));
        msg.insert(
            "thread_id".into(),
            json!(thread_id.filter(|t| !t.is_empty()).unwrap_or(r)),
        );
    } else if let Some(t) = thread_id.filter(|t| !t.is_empty()) {
        msg.insert("thread_id".into(), json!(t));
    }
    msg
}

/// `-` reads the body from stdin: no shell quoting, nothing in argv/ps.
fn body_arg(raw: &str) -> String {
    if raw == "-" {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .unwrap_or_else(|e| die(&format!("agent-radio: {e}")));
        buf
    } else {
        raw.to_string()
    }
}

fn find_by_view_number(s: &Store, agent: &str, number: usize) -> Map<String, Value> {
    let ids = load_view(s, agent);
    if number < 1 || number > ids.len() {
        die(&format!("agent-radio: no message #{number} in last view"));
    }
    let wanted = &ids[number - 1];
    for msg in load_messages(s) {
        if str_field(&msg, "id") == wanted {
            return msg;
        }
    }
    die(&format!(
        "agent-radio: message #{number} is no longer available"
    ));
}

// ------------------------------------------------------------ commands --

fn cmd_send(args: &SendArgs) {
    let s = store();
    let sender = agent_from_env(args.sender.as_deref());
    let branch = match &args.branch {
        Some(b) => Some(b.clone()),
        None => current_branch(),
    };
    let body = body_arg(&args.body);
    let msg = make_message(
        &sender,
        validate_name(&args.to),
        &args.kind,
        &body,
        branch.as_deref(),
        &args.focus,
        args.risk.as_deref(),
        args.priority.as_deref(),
        None,
        None,
    );
    let mut msg = msg;
    validate_manifest_options(&args.manifest, args.no_manifest, args.manifest_auto);
    enforce_done_manifest_policy(
        str_field(&msg, "kind"),
        &args.manifest,
        args.no_manifest,
        args.manifest_auto,
    );
    insert_requested_manifest(
        &mut msg,
        &args.manifest,
        args.no_manifest,
        args.manifest_auto,
    );
    if let Some(t) = &args.task {
        if !t.is_empty() {
            msg.insert("task".into(), json!(t));
        }
    }
    {
        let _guard = locked(&s);
        let mut messages = load_messages(&s);
        append_message(&s, &msg);
        messages.push(msg.clone());
        set_notify_flags(&s, &msg, &messages);
    }
    println!(
        "sent {} {} -> {} #{}",
        str_field(&msg, "kind"),
        str_field(&msg, "from"),
        str_field(&msg, "to"),
        str_field(&msg, "id"),
    );
}

fn cmd_inbox(as_agent: Option<&str>, peek: bool) {
    let s = store();
    let agent = agent_from_env(as_agent);
    let messages;
    {
        let _guard = locked(&s);
        let mut seen = load_seen(&s, &agent);
        messages = unread_for(&s, &agent);
        let ids: Vec<String> = messages
            .iter()
            .map(|m| str_field(m, "id").to_string())
            .collect();
        save_view(&s, &agent, &ids);
        if !peek {
            seen.extend(ids);
            save_seen(&s, &agent, &seen);
            clear_notify_if_caught_up(&s, &agent, &unread_for(&s, &agent));
        }
    }
    if messages.is_empty() {
        println!("inbox for {agent}: empty");
    } else {
        print_rendered(&messages);
    }
}

fn cmd_history(
    as_agent: Option<&str>,
    limit: usize,
    with_agent: Option<&str>,
    branch: Option<&str>,
) {
    let s = store();
    let agent = as_agent.map(|a| agent_from_env(Some(a)));
    let mut messages = load_messages(&s);
    if let Some(w) = with_agent {
        messages.retain(|m| str_field(m, "from") == w || str_field(m, "to") == w);
    }
    if let Some(b) = branch {
        messages.retain(|m| str_field(m, "branch") == b);
    }
    let skip = messages.len().saturating_sub(limit);
    let messages: Vec<_> = messages.into_iter().skip(skip).collect();
    if let Some(agent) = agent {
        let _guard = locked(&s);
        let ids: Vec<String> = messages
            .iter()
            .map(|m| str_field(m, "id").to_string())
            .collect();
        save_view(&s, &agent, &ids);
    }
    if messages.is_empty() {
        println!("history: empty");
    } else {
        print_rendered(&messages);
    }
}

fn cmd_manifest_emit(task: Option<String>, paths: Vec<String>) {
    let paths = if paths.is_empty() {
        let p = git_status_files();
        if p.is_empty() {
            die("agent-radio: nothing to hash");
        }
        p
    } else {
        paths
    };
    let git_root = git_root();
    let mut files: BTreeMap<String, Option<String>> = BTreeMap::new();
    for path_str in &paths {
        let p = Path::new(path_str);
        let abs = if p.is_absolute() {
            p.to_path_buf()
        } else {
            std::env::current_dir().unwrap_or_default().join(p)
        };
        let rel = abs
            .strip_prefix(&git_root)
            .unwrap_or_else(|_| {
                die(&format!(
                    "agent-radio: path {path_str} is outside git worktree"
                ))
            })
            .to_string_lossy()
            .to_string();
        let hash = sha256_file(&abs);
        files.insert(rel, hash);
    }
    let digest = compute_manifest_digest(&files);
    let mut result = Map::new();
    result.insert("generated_at".into(), json!(utc_now()));
    if let Some(t) = task.filter(|t| !t.is_empty()) {
        result.insert("task".into(), json!(t));
    }
    result.insert("files".into(), json!(files));
    result.insert("digest".into(), json!(digest));
    println!("{}", serde_json::to_string_pretty(&result).unwrap());
}

fn cmd_manifest_verify(
    number: Option<usize>,
    task: Option<String>,
    strict: bool,
    ignore: &[String],
    as_agent: Option<String>,
) {
    let s = store();
    let msg;
    if let Some(n) = number {
        let agent = agent_from_env(as_agent.as_deref());
        let _guard = locked(&s);
        msg = find_by_view_number(&s, &agent, n);
    } else if let Some(t) = task {
        let msgs = load_messages(&s);
        msg = msgs
            .into_iter()
            .rev()
            .find(|m| str_field(m, "task") == t.as_str() && m.contains_key("manifest"))
            .unwrap_or_else(|| die(&format!("agent-radio: no manifest found for task '{t}'")));
    } else {
        die("agent-radio: specify a NUMBER or --task <id>");
    };

    let manifest = msg
        .get("manifest")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_else(|| die("agent-radio: message has no manifest"));
    let files = manifest
        .get("files")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_else(|| die("agent-radio: manifest has no files"));
    let reported_digest = manifest
        .get("digest")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let mut claimed_files: BTreeMap<String, Option<String>> = BTreeMap::new();
    for (path_str, hash_val) in &files {
        claimed_files.insert(path_str.clone(), hash_val.as_str().map(str::to_string));
    }
    let computed_digest = compute_manifest_digest(&claimed_files);
    if computed_digest != reported_digest {
        eprintln!("DIGEST corrupto (manifiesto editado a mano?)");
        exit(2);
    }

    let task_label = {
        let t = str_field(&msg, "task");
        if t.is_empty() {
            str_field(&msg, "from").to_string()
        } else {
            t.to_string()
        }
    };

    let git_root = git_root();
    let mut has_errors = false;
    let mut has_orphans = false;
    let mut all_claimed = BTreeSet::new();

    let mut sorted_paths: Vec<&String> = files.keys().collect();
    sorted_paths.sort();

    for path_str in sorted_paths {
        let hash_val = files.get(path_str).unwrap();
        let abs_path = git_root.join(path_str);
        all_claimed.insert(path_str.clone());

        let exists = abs_path.exists();
        let reported = hash_val.as_str();

        match (exists, reported) {
            (true, Some(rep)) => {
                let computed = sha256_file(&abs_path).unwrap_or_default();
                if computed == rep {
                    println!("OK {path_str}");
                } else {
                    println!(
                        "MISMATCH {path_str} (reportado {}… != disco {}…)",
                        &rep[..12.min(rep.len())],
                        &computed[..12.min(computed.len())]
                    );
                    has_errors = true;
                }
            }
            (true, None) => {
                println!("MISMATCH {path_str} (reportado (deleted) != disco present)");
                has_errors = true;
            }
            (false, Some(_rep)) => {
                println!("MISSING {path_str}");
                has_errors = true;
            }
            (false, None) => {
                println!("OK {path_str}");
            }
        }
    }

    if strict {
        let dirty = git_status_files();
        for d in dirty {
            if !all_claimed.contains(&d) && !ignore.iter().any(|pattern| glob_match(pattern, &d)) {
                eprintln!("HUERFANO {d}");
                has_orphans = true;
            }
        }
    }

    let status = if has_errors {
        "NO COINCIDE"
    } else if has_orphans {
        "HUERFANOS"
    } else {
        "VERIFICADO"
    };
    println!("-- tarea '{task_label}': {status}");

    if has_errors {
        exit(2);
    }
    if has_orphans {
        exit(3);
    }
}

fn cmd_manifest_map(limit: usize, strict: bool, ignore: &[String]) {
    let s = store();
    let all_msgs = load_messages(&s);

    let mut with_manifest: Vec<&Map<String, Value>> = all_msgs
        .iter()
        .filter(|m| m.contains_key("manifest"))
        .collect();
    with_manifest.reverse();

    let mut seen: BTreeMap<String, &Map<String, Value>> = BTreeMap::new();
    for msg in &with_manifest {
        let key = {
            let t = str_field(msg, "task");
            if t.is_empty() {
                let from = str_field(msg, "from");
                let id = str_field(msg, "id");
                let short = if id.len() > 8 { &id[..8] } else { id };
                format!("{from}#{short}")
            } else {
                t.to_string()
            }
        };
        seen.entry(key).or_insert(msg);
    }

    let entries: Vec<(&String, &&Map<String, Value>)> = seen.iter().take(limit).collect();

    let git_root = git_root();
    let mut all_ok = true;
    let mut has_orphans = false;
    let mut all_claimed: BTreeSet<String> = BTreeSet::new();

    println!("TASK | FROM | KIND | TS | FILES | DIGEST | ESTADO");
    println!("-----|------|----|---|-----|------|-------");

    for (task_key, msg) in &entries {
        let manifest = msg.get("manifest").and_then(Value::as_object).unwrap();
        let files = manifest.get("files").and_then(Value::as_object).unwrap();
        let n_files = files.len();
        let digest = manifest.get("digest").and_then(Value::as_str).unwrap_or("");
        let short_digest = if digest.len() > 8 {
            &digest[..8]
        } else {
            digest
        };

        let mut ok = true;
        let mut paths = Vec::new();
        for (path_str, hash_val) in files {
            let abs_path = git_root.join(path_str);
            all_claimed.insert(path_str.to_string());
            paths.push(path_str.to_string());
            let exists = abs_path.exists();
            let reported = hash_val.as_str();
            match (exists, reported) {
                (true, Some(rep)) => {
                    let computed = sha256_file(&abs_path).unwrap_or_default();
                    if computed != rep {
                        ok = false;
                    }
                }
                (true, None) | (false, Some(_)) => {
                    ok = false;
                }
                (false, None) => {}
            }
        }
        if !ok {
            all_ok = false;
        }
        paths.sort();
        let estado = if ok { "VERIFICADO" } else { "NO COINCIDE" };
        let suffix = if paths.is_empty() {
            String::new()
        } else {
            format!(" {}", paths.join(","))
        };

        println!(
            "{} | {} | {} | {} | {} | {} | {}{}",
            task_key,
            str_field(msg, "from"),
            str_field(msg, "kind"),
            sanitize(str_field(msg, "ts")),
            n_files,
            short_digest,
            estado,
            suffix,
        );
    }

    if strict {
        let dirty = git_status_files();
        for d in dirty {
            if !all_claimed.contains(&d) && !ignore.iter().any(|pattern| glob_match(pattern, &d)) {
                eprintln!("HUERFANO {d}");
                has_orphans = true;
            }
        }
    }

    if has_orphans {
        exit(3);
    }
    if !all_ok {
        exit(2);
    }
}

fn cmd_reply_kind(
    kind: &str,
    number: usize,
    as_agent: Option<&str>,
    body: &str,
    manifest: &[String],
    no_manifest: bool,
    manifest_auto: bool,
) {
    let s = store();
    let me = agent_from_env(as_agent);
    let mut msg;
    let original_id;
    {
        let _guard = locked(&s);
        let original = find_by_view_number(&s, &me, number);
        let raw_body = body_arg(body);
        let trimmed = raw_body.trim();
        let final_body = if trimmed.is_empty() {
            kind.to_lowercase()
        } else {
            trimmed.to_string()
        };
        let target = if str_field(&original, "from") == me {
            str_field(&original, "to").to_string()
        } else {
            str_field(&original, "from").to_string()
        };
        original_id = str_field(&original, "id").to_string();
        let branch = str_field(&original, "branch").to_string();
        let thread = str_field(&original, "thread_id").to_string();
        msg = make_message(
            &me,
            validate_name(&target),
            kind,
            &final_body,
            Some(&branch).filter(|b| !b.is_empty()).map(String::as_str),
            &[],
            None,
            None,
            Some(&original_id),
            Some(&thread).filter(|t| !t.is_empty()).map(String::as_str),
        );
        validate_manifest_options(manifest, no_manifest, manifest_auto);
        enforce_done_manifest_policy(kind, manifest, no_manifest, manifest_auto);
        insert_requested_manifest(&mut msg, manifest, no_manifest, manifest_auto);
        let mut messages = load_messages(&s);
        append_message(&s, &msg);
        messages.push(msg.clone());
        set_notify_flags(&s, &msg, &messages);
    }
    println!(
        "sent {kind} re #{original_id} -> {} #{}",
        str_field(&msg, "to"),
        str_field(&msg, "id"),
    );
}

fn cmd_team() {
    let mut agents: BTreeMap<String, String> = BTreeMap::new();
    for msg in load_messages(&store()) {
        let sender = str_field(&msg, "from");
        if !sender.is_empty() {
            agents.insert(sender.to_string(), str_field(&msg, "ts").to_string());
        }
        let to = str_field(&msg, "to");
        if !to.is_empty() && to != "all" {
            agents.entry(to.to_string()).or_default();
        }
    }
    if agents.is_empty() {
        println!("team: empty");
        return;
    }
    for (name, ts) in agents {
        println!("{name}\t{ts}");
    }
}

fn cmd_status(as_agent: Option<&str>, quiet: bool) {
    let s = store();
    let agent = agent_from_env(as_agent);
    let unread_count;
    let flagged;
    {
        let _guard = locked(&s);
        let unread = unread_for(&s, &agent);
        if unread.is_empty() {
            clear_notify_if_caught_up(&s, &agent, &unread);
            flagged = false;
        } else {
            flagged = notify_path(&s, &agent).exists();
        }
        unread_count = unread.len();
    }
    if quiet {
        exit(if unread_count > 0 { 0 } else { 1 });
    }
    println!(
        "{}",
        json!({ "agent": agent, "flag": flagged, "unread": unread_count })
    );
}

fn cmd_wait(as_agent: Option<&str>, timeout: f64, interval: f64) {
    let s = store();
    let agent = agent_from_env(as_agent);
    let deadline = Instant::now() + Duration::from_secs_f64(timeout.max(0.0));
    loop {
        {
            let _guard = locked(&s);
            let unread = unread_for(&s, &agent);
            if !unread.is_empty() {
                let ids: Vec<String> = unread
                    .iter()
                    .map(|m| str_field(m, "id").to_string())
                    .collect();
                save_view(&s, &agent, &ids);
                print_rendered(&unread);
                return;
            }
            clear_notify_if_caught_up(&s, &agent, &unread);
        }
        if Instant::now() >= deadline {
            exit(1);
        }
        std::thread::sleep(Duration::from_secs_f64(interval.max(0.05)));
    }
}

// ------------------------------------------------------------------ cli --

#[derive(Parser)]
#[command(name = "agent-radio", about = "Local-only persistent agent messages")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Args)]
struct SendArgs {
    /// sender identity (default: AGENT_RADIO_AGENT)
    #[arg(long = "from")]
    sender: Option<String>,
    #[arg(long)]
    to: String,
    #[arg(long, default_value = "ASK")]
    kind: String,
    /// message text; '-' reads stdin
    #[arg(long)]
    body: String,
    /// default: current git branch; pass '' to omit
    #[arg(long)]
    branch: Option<String>,
    /// concrete files/paths this message is about (repeatable)
    #[arg(long)]
    focus: Vec<String>,
    #[arg(long)]
    risk: Option<String>,
    #[arg(long, value_parser = ["low", "normal", "high", "urgent"])]
    priority: Option<String>,
    /// task identifier
    #[arg(long)]
    task: Option<String>,
    /// paths to include in manifest (hashed at send time)
    #[arg(long, conflicts_with_all = ["no_manifest", "manifest_auto"])]
    manifest: Vec<String>,
    /// explicitly declare no manifest for this message
    #[arg(long, conflicts_with_all = ["manifest", "manifest_auto"])]
    no_manifest: bool,
    /// include a manifest for all dirty files from git status
    #[arg(long, conflicts_with_all = ["manifest", "no_manifest"])]
    manifest_auto: bool,
}

#[derive(clap::Args)]
struct ReplyArgs {
    /// message # from the last inbox/history view
    number: usize,
    #[arg(long = "as")]
    as_agent: Option<String>,
    /// reply text; '-' reads stdin
    #[arg(long, default_value = "")]
    body: String,
    /// paths to include in manifest (hashed at reply time)
    #[arg(long, conflicts_with_all = ["no_manifest", "manifest_auto"])]
    manifest: Vec<String>,
    /// explicitly declare no manifest for this reply
    #[arg(long, conflicts_with_all = ["manifest", "manifest_auto"])]
    no_manifest: bool,
    /// include a manifest for all dirty files from git status
    #[arg(long, conflicts_with_all = ["manifest", "no_manifest"])]
    manifest_auto: bool,
}

#[derive(clap::Args)]
struct ManifestEmitArgs {
    /// task identifier
    #[arg(long)]
    task: Option<String>,
    /// paths to hash (default: files from git status)
    paths: Vec<String>,
}

#[derive(clap::Args)]
struct ManifestVerifyArgs {
    /// message number from last view
    #[arg(conflicts_with = "task")]
    number: Option<usize>,
    /// task id to search (most recent manifest for that task)
    #[arg(long, conflicts_with = "number")]
    task: Option<String>,
    /// also check for unclaimed dirty files in worktree
    #[arg(long)]
    strict: bool,
    /// glob pattern to exclude from strict orphan checks (repeatable)
    #[arg(long)]
    ignore: Vec<String>,
    /// agent identity (required with NUMBER)
    #[arg(long = "as")]
    as_agent: Option<String>,
}

#[derive(clap::Args)]
struct ManifestMapArgs {
    #[arg(long, default_value_t = 30)]
    limit: usize,
    #[arg(long)]
    strict: bool,
    /// glob pattern to exclude from strict orphan checks (repeatable)
    #[arg(long)]
    ignore: Vec<String>,
}

#[derive(Subcommand)]
enum ManifestCmd {
    /// Hash file paths and emit manifest JSON
    Emit(ManifestEmitArgs),
    /// Verify a message's manifest against the worktree
    Verify(ManifestVerifyArgs),
    /// Show table of all manifests across messages
    Map(ManifestMapArgs),
}

#[derive(Subcommand)]
enum Cmd {
    /// send a typed message
    Send(SendArgs),
    /// show unread messages for an agent
    Inbox {
        #[arg(long = "as")]
        as_agent: Option<String>,
        /// do not mark messages read
        #[arg(long)]
        peek: bool,
    },
    /// show recent messages
    History {
        /// save numbering for replies
        #[arg(long = "as")]
        as_agent: Option<String>,
        #[arg(long, default_value_t = 30)]
        limit: usize,
        #[arg(long = "with")]
        with_agent: Option<String>,
        #[arg(long)]
        branch: Option<String>,
    },
    /// reply to a numbered message with ACK
    #[command(alias = "reply")]
    Ack(ReplyArgs),
    /// reply to a numbered message with DONE
    Done(ReplyArgs),
    /// reply to a numbered message with DECLINE
    Decline(ReplyArgs),
    /// reply to a numbered message with FAILURE
    Failure(ReplyArgs),
    /// Create and verify file manifests
    #[command(subcommand)]
    Manifest(ManifestCmd),
    /// list known agents
    Team,
    /// show unread count and notify flag
    Status {
        #[arg(long = "as")]
        as_agent: Option<String>,
        /// exit 0 when unread exists, 1 otherwise
        #[arg(long)]
        quiet: bool,
    },
    /// wait until unread messages arrive
    Wait {
        #[arg(long = "as")]
        as_agent: Option<String>,
        #[arg(long, default_value_t = 300.0)]
        timeout: f64,
        #[arg(long, default_value_t = 2.0)]
        interval: f64,
    },
}

fn main() {
    match Cli::parse().cmd {
        Cmd::Send(args) => cmd_send(&args),
        Cmd::Inbox { as_agent, peek } => cmd_inbox(as_agent.as_deref(), peek),
        Cmd::History {
            as_agent,
            limit,
            with_agent,
            branch,
        } => cmd_history(
            as_agent.as_deref(),
            limit,
            with_agent.as_deref(),
            branch.as_deref(),
        ),
        Cmd::Ack(a) => cmd_reply_kind(
            "ACK",
            a.number,
            a.as_agent.as_deref(),
            &a.body,
            &a.manifest,
            a.no_manifest,
            a.manifest_auto,
        ),
        Cmd::Done(a) => {
            cmd_reply_kind(
                "DONE",
                a.number,
                a.as_agent.as_deref(),
                &a.body,
                &a.manifest,
                a.no_manifest,
                a.manifest_auto,
            );
        }
        Cmd::Decline(a) => cmd_reply_kind(
            "DECLINE",
            a.number,
            a.as_agent.as_deref(),
            &a.body,
            &a.manifest,
            a.no_manifest,
            a.manifest_auto,
        ),
        Cmd::Failure(a) => cmd_reply_kind(
            "FAILURE",
            a.number,
            a.as_agent.as_deref(),
            &a.body,
            &a.manifest,
            a.no_manifest,
            a.manifest_auto,
        ),
        Cmd::Manifest(command) => match command {
            ManifestCmd::Emit(a) => cmd_manifest_emit(a.task, a.paths),
            ManifestCmd::Verify(a) => {
                cmd_manifest_verify(a.number, a.task, a.strict, &a.ignore, a.as_agent);
            }
            ManifestCmd::Map(a) => cmd_manifest_map(a.limit, a.strict, &a.ignore),
        },
        Cmd::Team => cmd_team(),
        Cmd::Status { as_agent, quiet } => cmd_status(as_agent.as_deref(), quiet),
        Cmd::Wait {
            as_agent,
            timeout,
            interval,
        } => cmd_wait(as_agent.as_deref(), timeout, interval),
    }
}
