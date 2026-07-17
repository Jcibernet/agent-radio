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
//!   AGENT_RADIO_CLIENT_ID stable session id used to assign a human name
//!   AGENT_RADIO_PROVIDER  optional provider metadata (claude, opencode, ...)

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{exit, Command};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::LazyLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use clap::{Parser, Subcommand};
use regex::Regex;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use time::format_description::BorrowedFormatItem;
use time::macros::format_description;
use time::OffsetDateTime;

const TS_FORMAT: &[BorrowedFormatItem<'_>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:6]Z");
const MAX_BODY_BYTES: usize = 256 * 1024;
const MAX_STORE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_HASH_FILE_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_MESSAGE_LINE_BYTES: usize = 1024 * 1024;
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

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

const HUMAN_NAMES: [&str; 26] = [
    "Alice", "Bob", "Charlie", "Diana", "Ethan", "Fiona", "George", "Hannah", "Isaac", "Julia",
    "Kevin", "Laura", "Michael", "Nora", "Oliver", "Penny", "Quentin", "Rachel", "Samuel", "Tina",
    "Ursula", "Victor", "Wendy", "Xavier", "Yvonne", "Zachary",
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
    agents: PathBuf,
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
        agents: root.join("agents.json"),
        lock: root.join("lock"),
        seen_dir: root.join("seen"),
        views_dir: root.join("views"),
        notify_dir: root.join("notify"),
        root,
    }
}

fn secure_dir(path: &Path) {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                die(&format!(
                    "agent-radio: refusing unsafe store directory {}",
                    path.display()
                ));
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(path).unwrap_or_else(|e| die(&format!("agent-radio: {e}")));
        }
        Err(e) => die(&format!("agent-radio: {e}")),
    }
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .unwrap_or_else(|e| die(&format!("agent-radio: {e}")));
}

fn is_valid_name(name: &str) -> bool {
    NAME_RE.is_match(name)
}

fn reject_unsafe_file(path: &Path) {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                die(&format!(
                    "agent-radio: refusing unsafe store file {}",
                    path.display()
                ));
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => die(&format!("agent-radio: {e}")),
    }
}

fn harden_file_permissions(file: &fs::File) {
    #[cfg(unix)]
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .unwrap_or_else(|e| die(&format!("agent-radio: {e}")));
    #[cfg(not(unix))]
    let _ = file;
}

#[cfg(unix)]
fn set_nofollow(options: &mut fs::OpenOptions) {
    options.custom_flags(libc::O_NOFOLLOW);
}

#[cfg(not(unix))]
fn set_nofollow(_options: &mut fs::OpenOptions) {}

fn read_bounded_regular(path: &Path, max_bytes: u64) -> Option<String> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    set_nofollow(&mut options);
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => die(&format!("agent-radio: {e}")),
    };
    let metadata = file
        .metadata()
        .unwrap_or_else(|e| die(&format!("agent-radio: {e}")));
    if !metadata.is_file() {
        die(&format!(
            "agent-radio: refusing unsafe store file {}",
            path.display()
        ));
    }
    if metadata.len() > max_bytes {
        die(&format!(
            "agent-radio: store file {} exceeds {} bytes",
            path.display(),
            max_bytes
        ));
    }
    let mut text = String::new();
    file.read_to_string(&mut text)
        .unwrap_or_else(|e| die(&format!("agent-radio: {e}")));
    Some(text)
}

fn write_atomic_bytes(dir: &Path, name: &str, bytes: &[u8]) {
    if Path::new(name).components().count() != 1 {
        die("agent-radio: invalid sidecar filename");
    }
    secure_dir(dir);
    let path = dir.join(name);
    reject_unsafe_file(&path);
    let nonce = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
            ^ u128::from(nonce)
    ));
    let mut options = fs::OpenOptions::new();
    set_nofollow(&mut options);
    options.create_new(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(&tmp)
        .unwrap_or_else(|e| die(&format!("agent-radio: {e}")));
    harden_file_permissions(&file);
    if let Err(e) = file.write_all(bytes).and_then(|_| file.sync_all()) {
        let _ = fs::remove_file(&tmp);
        die(&format!("agent-radio: {e}"));
    }
    drop(file);
    if let Err(e) = fs::rename(&tmp, &path) {
        let _ = fs::remove_file(&tmp);
        die(&format!("agent-radio: {e}"));
    }
}

/// Exclusive advisory lock on the store; released when the guard drops.
struct LockGuard(fs::File);

fn locked(s: &Store) -> LockGuard {
    secure_dir(&s.root);
    secure_dir(&s.seen_dir);
    secure_dir(&s.views_dir);
    secure_dir(&s.notify_dir);
    reject_unsafe_file(&s.lock);
    // read+write (not append-only): Windows' LockFileEx rejects handles
    // without real read/write access. Never truncated or written.
    let mut options = fs::OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    set_nofollow(&mut options);
    let file = options
        .open(&s.lock)
        .unwrap_or_else(|e| die(&format!("agent-radio: {e}")));
    harden_file_permissions(&file);
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
    if !is_valid_name(name) {
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
    if let Some(name) = explicit.map(str::to_string).or_else(|| {
        std::env::var("AGENT_RADIO_AGENT")
            .ok()
            .filter(|v| !v.is_empty())
    }) {
        validate_name(&name);
        return resolve_agent_name(&store(), &name);
    }
    if let Ok(client_id) = std::env::var("AGENT_RADIO_CLIENT_ID") {
        if !client_id.is_empty() {
            let provider = std::env::var("AGENT_RADIO_PROVIDER")
                .ok()
                .filter(|v| !v.is_empty());
            return register_agent(&store(), &client_id, provider.as_deref());
        }
    }
    die(
        "agent-radio: pass --as/--from, set AGENT_RADIO_AGENT, or set \
         AGENT_RADIO_CLIENT_ID for automatic human naming",
    )
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

#[derive(Clone)]
struct RegisteredAgent {
    ordinal: u64,
    name: String,
    display_name: Option<String>,
    aliases: Vec<String>,
    provider: Option<String>,
    created_at: String,
}

fn empty_registry() -> Value {
    json!({ "next_ordinal": 0, "agents": {} })
}

fn load_registry(s: &Store) -> Value {
    let Some(text) = read_bounded_regular(&s.agents, MAX_STORE_BYTES) else {
        return empty_registry();
    };
    let value: Value = serde_json::from_str(&text)
        .unwrap_or_else(|_| die("agent-radio: agents registry is corrupt"));
    if value.get("next_ordinal").and_then(Value::as_u64).is_none()
        || value.get("agents").and_then(Value::as_object).is_none()
    {
        die("agent-radio: agents registry is corrupt");
    }
    value
}

fn load_registered_agents(s: &Store) -> Vec<RegisteredAgent> {
    let registry = load_registry(s);
    let records = registry
        .get("agents")
        .and_then(Value::as_object)
        .unwrap_or_else(|| die("agent-radio: agents registry is corrupt"));
    let mut agents: Vec<RegisteredAgent> = records
        .values()
        .map(|record| {
            let ordinal = record
                .get("ordinal")
                .and_then(Value::as_u64)
                .unwrap_or_else(|| die("agent-radio: agents registry is corrupt"));
            let name = record
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_else(|| die("agent-radio: agents registry is corrupt"))
                .to_string();
            validate_name(&name);
            let display_name = record
                .get("display_name")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_string);
            if let Some(value) = &display_name {
                validate_name(value);
            }
            let aliases = record
                .get("aliases")
                .and_then(Value::as_array)
                .map(|values| {
                    values
                        .iter()
                        .map(|value| {
                            let alias = value
                                .as_str()
                                .unwrap_or_else(|| die("agent-radio: agents registry is corrupt"))
                                .to_string();
                            validate_name(&alias);
                            alias
                        })
                        .collect()
                })
                .unwrap_or_default();
            let provider = record
                .get("provider")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_string);
            if let Some(value) = &provider {
                validate_name(value);
            }
            let created_at = record
                .get("created_at")
                .and_then(Value::as_str)
                .unwrap_or_else(|| die("agent-radio: agents registry is corrupt"))
                .to_string();
            RegisteredAgent {
                ordinal,
                name,
                display_name,
                aliases,
                provider,
                created_at,
            }
        })
        .collect();
    agents.sort_by_key(|agent| agent.ordinal);
    let mut names = BTreeSet::new();
    for agent in &agents {
        for name in std::iter::once(&agent.name).chain(agent.aliases.iter()) {
            if !names.insert(name.to_ascii_lowercase()) {
                die("agent-radio: agents registry contains duplicate names");
            }
        }
    }
    agents
}

fn human_name(ordinal: u64) -> String {
    let index = ordinal as usize % HUMAN_NAMES.len();
    let generation = ordinal as usize / HUMAN_NAMES.len();
    if generation == 0 {
        HUMAN_NAMES[index].to_string()
    } else {
        format!("{}-{}", HUMAN_NAMES[index], generation + 1)
    }
}

fn displayed_name(agent: &RegisteredAgent) -> &str {
    agent.display_name.as_deref().unwrap_or(&agent.name)
}

fn display_from_map<'a>(names: &'a BTreeMap<String, String>, canonical: &'a str) -> &'a str {
    names
        .get(canonical)
        .map(String::as_str)
        .unwrap_or(canonical)
}

fn resolve_agent_name(s: &Store, input: &str) -> String {
    if input == "all" {
        return input.to_string();
    }
    for agent in load_registered_agents(s) {
        if agent.name == input || agent.aliases.iter().any(|alias| alias == input) {
            return agent.name;
        }
    }
    input.to_string()
}

fn display_agent_name(s: &Store, canonical: &str) -> String {
    load_registered_agents(s)
        .into_iter()
        .find(|agent| agent.name == canonical)
        .map(|agent| displayed_name(&agent).to_string())
        .unwrap_or_else(|| canonical.to_string())
}

fn is_generated_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    HUMAN_NAMES.iter().any(|base| {
        let base = base.to_ascii_lowercase();
        lower == base
            || lower
                .strip_prefix(&(base + "-"))
                .is_some_and(|suffix| suffix.parse::<u64>().is_ok_and(|value| value >= 2))
    })
}

fn rename_agent(s: &Store, client_id: &str, new_name: Option<&str>) -> (String, String) {
    if client_id.is_empty() {
        die("agent-radio: client id must not be empty");
    }
    if let Some(name) = new_name {
        validate_name(name);
        if name.chars().count() > 32 {
            die("agent-radio: custom agent names are limited to 32 characters");
        }
        if name.eq_ignore_ascii_case("all") {
            die("agent-radio: 'all' is reserved for broadcasts");
        }
    }

    let client_key = sha256_bytes(client_id.as_bytes());
    let _guard = locked(s);
    let mut registry = load_registry(s);
    let registered = load_registered_agents(s);
    let current = registered
        .iter()
        .find(|agent| {
            registry
                .get("agents")
                .and_then(Value::as_object)
                .and_then(|records| records.get(&client_key))
                .and_then(|record| record.get("name"))
                .and_then(Value::as_str)
                == Some(agent.name.as_str())
        })
        .cloned()
        .unwrap_or_else(|| die("agent-radio: register this client id before renaming it"));
    let target = new_name
        .filter(|name| !name.eq_ignore_ascii_case(&current.name))
        .map(|name| {
            current
                .aliases
                .iter()
                .find(|alias| alias.eq_ignore_ascii_case(name))
                .cloned()
                .unwrap_or_else(|| name.to_string())
        });

    if let Some(name) = target.as_deref() {
        if is_generated_name(name) {
            die("agent-radio: automatically assigned names are reserved");
        }
        let owned_by_current = current
            .aliases
            .iter()
            .any(|alias| alias.eq_ignore_ascii_case(name));
        let collides_with_registered = registered.iter().any(|agent| {
            agent.name.eq_ignore_ascii_case(name)
                || agent
                    .aliases
                    .iter()
                    .any(|alias| alias.eq_ignore_ascii_case(name))
        });
        if collides_with_registered && !owned_by_current {
            die("agent-radio: that agent name is already in use");
        }
        let collides_with_legacy =
            known_agents_from_messages(&load_messages(s))
                .iter()
                .any(|legacy| {
                    legacy.eq_ignore_ascii_case(name) && !legacy.eq_ignore_ascii_case(&current.name)
                });
        if collides_with_legacy {
            die("agent-radio: that agent name is already present in message history");
        }
    }

    let record = registry
        .get_mut("agents")
        .and_then(Value::as_object_mut)
        .and_then(|records| records.get_mut(&client_key))
        .and_then(Value::as_object_mut)
        .unwrap_or_else(|| die("agent-radio: agents registry is corrupt"));
    match target.as_deref() {
        Some(name) => {
            let aliases = record
                .entry("aliases")
                .or_insert_with(|| json!([]))
                .as_array_mut()
                .unwrap_or_else(|| die("agent-radio: agents registry is corrupt"));
            if !aliases
                .iter()
                .filter_map(Value::as_str)
                .any(|alias| alias.eq_ignore_ascii_case(name))
            {
                aliases.push(json!(name));
            }
            record.insert("display_name".into(), json!(name));
        }
        None => {
            record.insert("display_name".into(), json!(""));
        }
    }
    write_json_atomic(&s.root, "agents.json", &registry);
    let display = target
        .as_deref()
        .unwrap_or(current.name.as_str())
        .to_string();
    (current.name, display)
}

fn register_agent(s: &Store, client_id: &str, provider: Option<&str>) -> String {
    if client_id.is_empty() {
        die("agent-radio: client id must not be empty");
    }
    if let Some(value) = provider {
        validate_name(value);
    }
    let client_key = sha256_bytes(client_id.as_bytes());
    let _guard = locked(s);
    let mut registry = load_registry(s);
    if let Some(name) = registry
        .get("agents")
        .and_then(Value::as_object)
        .and_then(|agents| agents.get(&client_key))
        .and_then(|record| record.get("name"))
        .and_then(Value::as_str)
    {
        return name.to_string();
    }

    let mut used: BTreeSet<String> = load_registered_agents(s)
        .into_iter()
        .map(|agent| agent.name)
        .collect();
    used.extend(known_agents_from_messages(&load_messages(s)));
    let mut ordinal = registry
        .get("next_ordinal")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| die("agent-radio: agents registry is corrupt"));
    let name = loop {
        let candidate = human_name(ordinal);
        ordinal += 1;
        if !used.contains(&candidate) {
            break candidate;
        }
    };
    let record = json!({
        "ordinal": ordinal - 1,
        "name": name,
        "display_name": "",
        "aliases": [],
        "provider": provider.unwrap_or(""),
        "created_at": utc_now(),
    });
    registry["next_ordinal"] = json!(ordinal);
    registry
        .get_mut("agents")
        .and_then(Value::as_object_mut)
        .unwrap_or_else(|| die("agent-radio: agents registry is corrupt"))
        .insert(client_key, record);
    write_json_atomic(&s.root, "agents.json", &registry);
    name
}

fn sha256_file(path: &Path) -> Option<String> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    set_nofollow(&mut options);
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => die(&format!("agent-radio: {e}")),
    };
    let metadata = file
        .metadata()
        .unwrap_or_else(|e| die(&format!("agent-radio: {e}")));
    if !metadata.is_file() {
        die(&format!(
            "agent-radio: refusing to hash non-regular file {}",
            path.display()
        ));
    }
    if metadata.len() > MAX_HASH_FILE_BYTES {
        die(&format!(
            "agent-radio: file {} exceeds {} bytes",
            path.display(),
            MAX_HASH_FILE_BYTES
        ));
    }
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .unwrap_or_else(|e| die(&format!("agent-radio: {e}")));
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Some(
        hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    )
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

fn canonicalize_allow_missing(path: &Path) -> PathBuf {
    let mut existing = path.to_path_buf();
    let mut missing = Vec::new();
    loop {
        match existing.try_exists() {
            Ok(true) => break,
            Ok(false) => {
                let name = existing
                    .file_name()
                    .unwrap_or_else(|| die("agent-radio: invalid manifest path"))
                    .to_os_string();
                missing.push(name);
                if !existing.pop() {
                    die("agent-radio: invalid manifest path");
                }
            }
            Err(e) => die(&format!("agent-radio: {e}")),
        }
    }
    let mut resolved =
        fs::canonicalize(existing).unwrap_or_else(|e| die(&format!("agent-radio: {e}")));
    for name in missing.into_iter().rev() {
        resolved.push(name);
    }
    resolved
}

fn safe_manifest_path(git_root: &Path, path_str: &str) -> PathBuf {
    let path = Path::new(path_str);
    if path_str.is_empty()
        || path_str.len() > 4096
        || path_str.contains('\\')
        || path.is_absolute()
        || !path
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
    {
        die(&format!("agent-radio: unsafe manifest path {path_str:?}"));
    }
    let resolved = canonicalize_allow_missing(&git_root.join(path));
    if resolved.strip_prefix(git_root).is_err() {
        die(&format!(
            "agent-radio: manifest path {path_str:?} escapes git worktree"
        ));
    }
    resolved
}

fn manifest_files(manifest: &Map<String, Value>) -> Option<BTreeMap<String, Option<String>>> {
    let raw_files = manifest.get("files")?.as_object()?;
    if raw_files.len() > 10_000 {
        return None;
    }
    let mut files = BTreeMap::new();
    for (path, value) in raw_files {
        let hash = match value {
            Value::Null => None,
            Value::String(hash)
                if hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()) =>
            {
                Some(hash.to_ascii_lowercase())
            }
            _ => return None,
        };
        files.insert(path.clone(), hash);
    }
    Some(files)
}

fn collect_manifest_files(paths: &[String]) -> BTreeMap<String, Option<String>> {
    let git_root =
        fs::canonicalize(git_root()).unwrap_or_else(|e| die(&format!("agent-radio: {e}")));
    let cwd = std::env::current_dir()
        .and_then(fs::canonicalize)
        .unwrap_or_else(|e| die(&format!("agent-radio: {e}")));
    let mut files = BTreeMap::new();
    for path_str in paths {
        let path = Path::new(path_str);
        let candidate = if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        };
        let absolute = canonicalize_allow_missing(&candidate);
        let relative = absolute
            .strip_prefix(&git_root)
            .unwrap_or_else(|_| {
                die(&format!(
                    "agent-radio: path {path_str} is outside git worktree"
                ))
            })
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        files.insert(relative, sha256_file(&absolute));
    }
    files
}

/// Hash paths relative to cwd, store as relative to git root.
/// Returns Map suitable for inserting as "manifest" in a message.
fn make_manifest(paths: &[String]) -> Map<String, Value> {
    let files = collect_manifest_files(paths);
    let digest = compute_manifest_digest(&files);
    let mut manifest = Map::new();
    manifest.insert("files".into(), json!(files));
    manifest.insert("digest".into(), json!(digest));
    manifest
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

fn optional_string_within(message: &Map<String, Value>, key: &str, max: usize) -> bool {
    message
        .get(key)
        .is_none_or(|value| value.as_str().is_some_and(|text| text.len() <= max))
}

fn valid_loaded_message(message: &Map<String, Value>) -> bool {
    let Some(id) = message.get("id").and_then(Value::as_str) else {
        return false;
    };
    let Some(sender) = message.get("from").and_then(Value::as_str) else {
        return false;
    };
    let Some(recipient) = message.get("to").and_then(Value::as_str) else {
        return false;
    };
    let Some(kind) = message.get("kind").and_then(Value::as_str) else {
        return false;
    };
    let Some(body) = message.get("body").and_then(Value::as_str) else {
        return false;
    };
    if id.is_empty()
        || id.len() > 256
        || !is_valid_name(sender)
        || sender == "all"
        || !is_valid_name(recipient)
        || !KINDS.contains(&kind)
        || body.len() > MAX_BODY_BYTES
        || message
            .get("version")
            .is_some_and(|version| version.as_u64().is_none())
    {
        return false;
    }
    if ![
        ("ts", 128),
        ("branch", 4096),
        ("risk", 64 * 1024),
        ("priority", 128),
        ("reply_to", 256),
        ("thread_id", 256),
        ("task", 4096),
    ]
    .iter()
    .all(|(key, max)| optional_string_within(message, key, *max))
    {
        return false;
    }
    message.get("focus").is_none_or(|focus| {
        focus.as_array().is_some_and(|paths| {
            paths.len() <= 1024
                && paths
                    .iter()
                    .all(|path| path.as_str().is_some_and(|text| text.len() <= 4096))
        })
    })
}

fn load_messages(s: &Store) -> Vec<Map<String, Value>> {
    let Some(text) = read_bounded_regular(&s.messages, MAX_STORE_BYTES) else {
        return Vec::new();
    };
    let mut messages = Vec::new();
    let mut ids = BTreeSet::new();
    for line in text.lines() {
        if line.trim().is_empty() || line.len() > MAX_MESSAGE_LINE_BYTES {
            continue;
        }
        let Ok(Value::Object(message)) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if !valid_loaded_message(&message) {
            continue;
        }
        let id = str_field(&message, "id").to_string();
        if !ids.insert(id.clone()) {
            die(&format!("agent-radio: duplicate message id {id}"));
        }
        messages.push(message);
    }
    messages
}

fn append_message(s: &Store, msg: &Map<String, Value>) {
    secure_dir(&s.root);
    reject_unsafe_file(&s.messages);
    let mut options = fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    options.mode(0o600);
    set_nofollow(&mut options);
    let mut file = options
        .open(&s.messages)
        .unwrap_or_else(|e| die(&format!("agent-radio: {e}")));
    harden_file_permissions(&file);
    let line = serde_json::to_string(msg).unwrap_or_else(|e| die(&format!("agent-radio: {e}")));
    writeln!(file, "{line}").unwrap_or_else(|e| die(&format!("agent-radio: {e}")));
}

fn str_field<'a>(msg: &'a Map<String, Value>, key: &str) -> &'a str {
    msg.get(key).and_then(Value::as_str).unwrap_or("")
}

// --------------------------------------------------------------- notify --

fn notify_path(s: &Store, agent: &str) -> PathBuf {
    if !is_valid_name(agent) || agent == "all" {
        die("agent-radio: invalid notification recipient");
    }
    s.notify_dir.join(format!("{agent}.flag"))
}

fn known_agents_from_messages(messages: &[Map<String, Value>]) -> BTreeSet<String> {
    let mut agents = BTreeSet::new();
    for msg in messages {
        let sender = str_field(msg, "from");
        if is_valid_name(sender) && sender != "all" {
            agents.insert(sender.to_string());
        }
        let to = str_field(msg, "to");
        if is_valid_name(to) && to != "all" {
            agents.insert(to.to_string());
        }
    }
    agents
}

fn known_agents(s: &Store, messages: &[Map<String, Value>]) -> BTreeSet<String> {
    let mut agents = known_agents_from_messages(messages);
    agents.extend(
        load_registered_agents(s)
            .into_iter()
            .map(|agent| agent.name),
    );
    agents
}

fn set_notify_flags(s: &Store, msg: &Map<String, Value>, messages: &[Map<String, Value>]) {
    fs::create_dir_all(&s.notify_dir).unwrap_or_else(|e| die(&format!("agent-radio: {e}")));
    let to = str_field(msg, "to");
    let recipients: BTreeSet<String> = if to == "all" {
        let mut r = known_agents(s, messages);
        r.remove(str_field(msg, "from"));
        r
    } else if !to.is_empty() {
        BTreeSet::from([to.to_string()])
    } else {
        BTreeSet::new()
    };
    for agent in recipients {
        write_atomic_bytes(
            &s.notify_dir,
            &format!("{agent}.flag"),
            str_field(msg, "id").as_bytes(),
        );
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
    let Some(text) = read_bounded_regular(&path, MAX_STORE_BYTES) else {
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

fn write_json_atomic(dir: &Path, name: &str, value: &Value) {
    let text =
        serde_json::to_string_pretty(value).unwrap_or_else(|e| die(&format!("agent-radio: {e}")));
    write_atomic_bytes(dir, name, text.as_bytes());
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
    let Some(text) = read_bounded_regular(&path, MAX_STORE_BYTES) else {
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

/// Terminal-injection guard: rendered messages must never carry control or
/// invisible directionality characters into the reader's terminal.
/// Tabs/newlines become spaces; remaining whitespace collapses.
fn is_unsafe_format_char(character: char) -> bool {
    matches!(
        character,
        '\u{061c}'
            | '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{206f}'
            | '\u{feff}'
    )
}

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
        .filter(|character| !character.is_control() && !is_unsafe_format_char(*character))
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
fn render(s: &Store, messages: &[Map<String, Value>]) -> Vec<String> {
    let display_names: BTreeMap<String, String> = load_registered_agents(s)
        .into_iter()
        .map(|agent| (agent.name.clone(), displayed_name(&agent).to_string()))
        .collect();
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
            sanitize(display_from_map(&display_names, str_field(msg, "from"),)),
            sanitize(display_from_map(&display_names, str_field(msg, "to"))),
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
                let short_digest: String = m
                    .get("digest")
                    .and_then(Value::as_str)
                    .map(sanitize)
                    .unwrap_or_default()
                    .chars()
                    .take(8)
                    .collect();
                lines.push(format!("    [manifest {n_files} files @{short_digest}]"));
            }
        }
    }
    lines
}

fn print_rendered(s: &Store, messages: &[Map<String, Value>]) {
    println!("{}", render(s, messages).join("\n"));
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
    if body.len() > MAX_BODY_BYTES {
        die(&format!("agent-radio: body exceeds {MAX_BODY_BYTES} bytes"));
    }
    if focus.len() > 1024
        || focus.iter().any(|path| path.len() > 4096)
        || risk.is_some_and(|value| value.len() > 64 * 1024)
    {
        die("agent-radio: message metadata is too large");
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
            .take((MAX_BODY_BYTES + 1) as u64)
            .read_to_string(&mut buf)
            .unwrap_or_else(|e| die(&format!("agent-radio: {e}")));
        if buf.len() > MAX_BODY_BYTES {
            die(&format!("agent-radio: body exceeds {MAX_BODY_BYTES} bytes"));
        }
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
    validate_name(&args.to);
    let recipient = resolve_agent_name(&s, &args.to);
    let msg = make_message(
        &sender,
        &recipient,
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
        display_agent_name(&s, str_field(&msg, "from")),
        display_agent_name(&s, str_field(&msg, "to")),
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
        println!("inbox for {}: empty", display_agent_name(&s, &agent));
    } else {
        print_rendered(&s, &messages);
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
        validate_name(w);
        let canonical = resolve_agent_name(&s, w);
        messages.retain(|m| str_field(m, "from") == canonical || str_field(m, "to") == canonical);
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
        print_rendered(&s, &messages);
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
    let files = collect_manifest_files(&paths);
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
        .unwrap_or_else(|| die("agent-radio: message has no valid manifest"));
    let claimed_files =
        manifest_files(manifest).unwrap_or_else(|| die("agent-radio: manifest is corrupt"));
    let reported_digest = manifest.get("digest").and_then(Value::as_str).unwrap_or("");
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

    let git_root =
        fs::canonicalize(git_root()).unwrap_or_else(|e| die(&format!("agent-radio: {e}")));
    let mut has_errors = false;
    let mut has_orphans = false;
    let mut all_claimed = BTreeSet::new();

    for (path_str, reported_hash) in &claimed_files {
        let abs_path = safe_manifest_path(&git_root, path_str);
        all_claimed.insert(path_str.clone());
        let exists = abs_path
            .try_exists()
            .unwrap_or_else(|e| die(&format!("agent-radio: {e}")));

        match (exists, reported_hash.as_deref()) {
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
                let from = sanitize(str_field(msg, "from"));
                let id: String = str_field(msg, "id").chars().take(8).collect();
                format!("{from}#{id}")
            } else {
                t.to_string()
            }
        };
        seen.entry(key).or_insert(msg);
    }

    let entries: Vec<(&String, &&Map<String, Value>)> = seen.iter().take(limit).collect();

    let git_root =
        fs::canonicalize(git_root()).unwrap_or_else(|e| die(&format!("agent-radio: {e}")));
    let mut all_ok = true;
    let mut has_orphans = false;
    let mut all_claimed: BTreeSet<String> = BTreeSet::new();

    println!("TASK | FROM | KIND | TS | FILES | DIGEST | ESTADO");
    println!("-----|------|----|---|-----|------|-------");

    for (task_key, msg) in &entries {
        let Some(manifest) = msg.get("manifest").and_then(Value::as_object) else {
            println!(
                "{} | {} | {} | {} | 0 | - | CORRUPTO",
                sanitize(task_key),
                sanitize(str_field(msg, "from")),
                sanitize(str_field(msg, "kind")),
                sanitize(str_field(msg, "ts")),
            );
            all_ok = false;
            continue;
        };
        let Some(files) = manifest_files(manifest) else {
            println!(
                "{} | {} | {} | {} | 0 | - | CORRUPTO",
                sanitize(task_key),
                sanitize(str_field(msg, "from")),
                sanitize(str_field(msg, "kind")),
                sanitize(str_field(msg, "ts")),
            );
            all_ok = false;
            continue;
        };
        let digest = manifest.get("digest").and_then(Value::as_str).unwrap_or("");
        let digest_valid =
            digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit());
        let short_digest = if digest_valid { &digest[..8] } else { "-" };
        let mut ok = digest_valid && compute_manifest_digest(&files) == digest;
        let mut paths = Vec::new();
        for (path_str, reported_hash) in &files {
            let abs_path = safe_manifest_path(&git_root, path_str);
            all_claimed.insert(path_str.to_string());
            paths.push(sanitize(path_str));
            let exists = abs_path
                .try_exists()
                .unwrap_or_else(|e| die(&format!("agent-radio: {e}")));
            match (exists, reported_hash.as_deref()) {
                (true, Some(reported)) => {
                    if sha256_file(&abs_path).as_deref() != Some(reported) {
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
        let estado = if ok { "VERIFICADO" } else { "NO COINCIDE" };
        let suffix = if paths.is_empty() {
            String::new()
        } else {
            format!(" {}", paths.join(","))
        };

        println!(
            "{} | {} | {} | {} | {} | {} | {}{}",
            sanitize(task_key),
            sanitize(str_field(msg, "from")),
            sanitize(str_field(msg, "kind")),
            sanitize(str_field(msg, "ts")),
            files.len(),
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
        display_agent_name(&s, str_field(&msg, "to")),
        str_field(&msg, "id"),
    );
}

fn required_client_id(explicit: Option<String>, command: &str) -> String {
    explicit
        .or_else(|| std::env::var("AGENT_RADIO_CLIENT_ID").ok())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| {
            die(&format!(
                "agent-radio: {command} requires --client-id or AGENT_RADIO_CLIENT_ID"
            ))
        })
}

fn cmd_rename(client_id: Option<String>, name: Option<String>) {
    let s = store();
    let client_id = required_client_id(client_id, "rename");
    let (canonical, display) = rename_agent(&s, &client_id, name.as_deref());
    if canonical == display {
        println!("{canonical}");
    } else {
        println!("{display}\t{canonical}");
    }
}

fn cmd_team() {
    let s = store();
    let messages = load_messages(&s);
    let mut last_seen: BTreeMap<String, String> = BTreeMap::new();
    for msg in &messages {
        let sender = str_field(msg, "from");
        if !sender.is_empty() {
            last_seen.insert(sender.to_string(), str_field(msg, "ts").to_string());
        }
        let to = str_field(msg, "to");
        if !to.is_empty() && to != "all" {
            last_seen.entry(to.to_string()).or_default();
        }
    }

    let registered = load_registered_agents(&s);
    let registered_names: BTreeSet<String> =
        registered.iter().map(|agent| agent.name.clone()).collect();
    if registered.is_empty() && last_seen.is_empty() {
        println!("team: empty");
        return;
    }
    for agent in registered {
        println!(
            "{}\t{}\t{}\t{}",
            displayed_name(&agent),
            agent.provider.as_deref().unwrap_or("-"),
            last_seen
                .get(&agent.name)
                .filter(|value| !value.is_empty())
                .map(String::as_str)
                .unwrap_or(&agent.created_at),
            agent.name,
        );
    }
    for (name, ts) in last_seen {
        if !registered_names.contains(name.as_str()) {
            println!("{name}\t-\t{ts}\t{name}");
        }
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
    let display_name = display_agent_name(&s, &agent);
    println!(
        "{}",
        json!({
            "agent": agent,
            "display_name": display_name,
            "flag": flagged,
            "unread": unread_count,
        })
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
                print_rendered(&s, &unread);
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

#[derive(clap::Args)]
struct RegisterArgs {
    /// stable id for this agent session (default: AGENT_RADIO_CLIENT_ID)
    #[arg(long)]
    client_id: Option<String>,
    /// implementation metadata, not the routing identity
    #[arg(long)]
    provider: Option<String>,
}

#[derive(clap::Args)]
struct RenameArgs {
    /// stable id for this agent session (default: AGENT_RADIO_CLIENT_ID)
    #[arg(long)]
    client_id: Option<String>,
    /// custom display/routing alias
    #[arg(long, required_unless_present = "reset", conflicts_with = "reset")]
    name: Option<String>,
    /// restore the automatically assigned name
    #[arg(long, conflicts_with = "name")]
    reset: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// assign or recover a human name for an agent session
    Register(RegisterArgs),
    /// set a custom alias without changing the stable routing identity
    Rename(RenameArgs),
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
        Cmd::Register(a) => {
            let client_id = required_client_id(a.client_id, "register");
            let provider = a
                .provider
                .or_else(|| std::env::var("AGENT_RADIO_PROVIDER").ok())
                .filter(|value| !value.is_empty());
            let s = store();
            let canonical = register_agent(&s, &client_id, provider.as_deref());
            println!("{}", display_agent_name(&s, &canonical));
        }
        Cmd::Rename(a) => {
            cmd_rename(a.client_id, if a.reset { None } else { a.name });
        }
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
