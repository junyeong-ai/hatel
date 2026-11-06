//! Model responses read from the transcripts Claude Code writes, recorded as `claude.response`.
//!
//! A transcript is JSON Lines that only grows, so each read resumes at the byte after the last
//! complete line the previous read recorded. That position is kept per transcript under the state
//! directory, with the file's identity: a transcript replaced or shortened is read again from its
//! start. Every response is keyed by its API request id, so reading a line twice, two hooks reading
//! one transcript at once, or a forked session's copy of its parent's responses all leave one
//! record per request. Reading is therefore correct without the position, which only spares the
//! work; an unreadable position is read as none.
//!
//! A subagent's hook can run before its last response reaches its transcript, and no later event
//! names that transcript again. So a subagent's transcript is remembered under its session, and the
//! session's next turn or its end reads every remembered transcript along with its own.

use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write as _};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::registry::Registry;
use crate::{Config, Payload, ProjectRef, make_envelope};

pub const KIND: &str = "claude.response";

/// The lifecycle events after which a transcript holds responses not yet read: the end of a turn,
/// the end of a subagent (whose responses are in a transcript of its own), and the end of the
/// session.
pub const EVENTS: [&str; 3] = ["Stop", "SubagentStop", "SessionEnd"];

const SOURCE: &str = "claude_code.transcript";
const POSITIONS: &str = "transcripts";
/// The extension of a session's list of subagent transcripts, beside the positions.
const SUBAGENTS: &str = "subagents";

/// The transcripts an event leaves with responses to read. A subagent's end names its own
/// transcript and remembers it under the session; a turn's or the session's end names the
/// session's transcript and every subagent transcript remembered for it.
pub fn due(cfg: &Config, event_name: &str, event: &serde_json::Value) -> Vec<PathBuf> {
    let path = |key: &str| {
        event
            .get(key)
            .and_then(serde_json::Value::as_str)
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
    };
    let session = event
        .get("session_id")
        .and_then(serde_json::Value::as_str)
        .filter(|id| !id.is_empty());
    let positions = cfg.state_dir.join(POSITIONS);
    match event_name {
        "SubagentStop" => {
            let agent = path("agent_transcript_path");
            if let (Some(session), Some(agent)) = (session, &agent) {
                remember_subagent(&positions, session, agent);
            }
            agent.into_iter().collect()
        }
        "Stop" | "SessionEnd" => path("transcript_path")
            .into_iter()
            .chain(session.map_or_else(Vec::new, |s| subagents(&positions, s)))
            .collect(),
        _ => Vec::new(),
    }
}

fn subagents_path(positions: &Path, session: &str) -> PathBuf {
    let name = blake3::hash(session.as_bytes()).to_hex();
    positions.join(format!("{name}.{SUBAGENTS}"))
}

/// Append `agent` to the session's list: one short line through an append-only descriptor, so
/// subagents ending at once each keep their line.
fn remember_subagent(positions: &Path, session: &str, agent: &Path) {
    if subagents(positions, session)
        .iter()
        .any(|known| known == agent)
    {
        return;
    }
    let mut line = agent.as_os_str().as_encoded_bytes().to_vec();
    line.push(b'\n');
    let written = std::fs::create_dir_all(positions).and_then(|()| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(subagents_path(positions, session))?
            .write_all(&line)
    });
    if let Err(e) = written {
        // This subagent's responses are read now; only what it writes after this hook is missed.
        eprintln!("hatel hook: {}: {e}", positions.display());
    }
}

fn subagents(positions: &Path, session: &str) -> Vec<PathBuf> {
    let Ok(text) = std::fs::read_to_string(subagents_path(positions, session)) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = text
        .lines()
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

/// What one read of a transcript recorded.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Read {
    /// Responses this read recorded for the first time.
    pub recorded: usize,
    /// Complete lines that name an API request but are not a readable response.
    pub unreadable: usize,
}

/// Record the responses `transcript` gained since it was last read. `project` attributes them, as
/// it does every record the same hook event writes.
pub fn read(
    cfg: &Config,
    registry: &Registry,
    sink: &mut crate::SqliteSink,
    transcript: &Path,
    project: Option<&ProjectRef>,
) -> crate::Result<Read> {
    let io = |e: std::io::Error| crate::Error::Io(format!("{}: {e}", transcript.display()));
    let mut file = match File::open(transcript) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Read::default()),
        Err(e) => return Err(io(e)),
    };
    let metadata = file.metadata().map_err(io)?;
    let identity = FileId::of(&metadata);
    let positions = cfg.state_dir.join(POSITIONS);
    let start = Position::load(&positions, transcript)
        .filter(|p| p.file == identity && p.offset <= metadata.len())
        .map_or(0, |p| p.offset);
    if start == metadata.len() {
        return Ok(Read::default());
    }
    file.seek(SeekFrom::Start(start)).map_err(io)?;
    let mut reader = BufReader::new(file);
    let mut offset = start;
    let mut line = Vec::new();
    let mut batch = Vec::new();
    let mut outcome = Read::default();
    loop {
        line.clear();
        let length = reader.read_until(b'\n', &mut line).map_err(io)?;
        // A line without its newline is still being written; the next read starts there.
        let complete = line.last() == Some(&b'\n');
        if complete {
            offset += length as u64;
            match response(&line, cfg, registry, project) {
                Line::Response(env) => batch.push(*env),
                Line::Unreadable => outcome.unreadable += 1,
                Line::Other => {}
            }
        }
        if batch.len() >= crate::sink::WRITE_BATCH || (!complete && !batch.is_empty()) {
            outcome.recorded += sink.write_new(&batch)?.records;
            batch.clear();
            Position::save(&positions, transcript, identity, offset);
        }
        if !complete {
            break;
        }
    }
    if offset > start {
        Position::save(&positions, transcript, identity, offset);
    }
    Ok(outcome)
}

enum Line {
    Response(Box<crate::Envelope>),
    Unreadable,
    Other,
}

/// The fields of a transcript entry a response record is made of; every other field is content or
/// bookkeeping and is never parsed into the record.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Entry {
    #[serde(rename = "type")]
    entry_type: Option<String>,
    session_id: Option<String>,
    request_id: Option<String>,
    agent_id: Option<String>,
    timestamp: Option<String>,
    message: Option<Message>,
}

#[derive(Deserialize)]
struct Message {
    model: Option<String>,
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Usage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
}

fn response(line: &[u8], cfg: &Config, registry: &Registry, project: Option<&ProjectRef>) -> Line {
    // Responses are the entries that name an API request; most lines are content and are skipped
    // without being parsed.
    if !contains(line, b"\"requestId\"") {
        return Line::Other;
    }
    let Ok(entry) = serde_json::from_slice::<Entry>(line) else {
        return Line::Unreadable;
    };
    let request_id = entry.request_id.filter(|id| !id.is_empty());
    let (Some("assistant"), Some(request_id), Some(message)) =
        (entry.entry_type.as_deref(), request_id, entry.message)
    else {
        return Line::Other;
    };
    let Some(usage) = message.usage else {
        return Line::Other;
    };
    let tokens = [
        ("input_tokens", usage.input_tokens),
        ("output_tokens", usage.output_tokens),
        ("cache_read_tokens", usage.cache_read_input_tokens),
        ("cache_creation_tokens", usage.cache_creation_input_tokens),
    ];
    // A response the API billed reports tokens. Claude Code writes token-less entries for a request
    // it refused, and for the copy of a request a subagent inherits with its parent's history, so
    // recording one would claim the request before its billed entry is read.
    if tokens.iter().all(|(_, count)| count.unwrap_or(0) == 0) {
        return Line::Other;
    }
    let session_id = entry.session_id.filter(|id| !id.is_empty());
    let occurred = entry
        .timestamp
        .and_then(|ts| ts.parse::<jiff::Timestamp>().ok());
    let (Some(session_id), Some(occurred)) = (session_id, occurred) else {
        return Line::Unreadable;
    };
    let mut payload = Payload::new();
    payload.insert("session_id".into(), session_id.into());
    payload.insert("request_id".into(), request_id.clone().into());
    if let Some(model) = message.model {
        payload.insert("model".into(), model.into());
    }
    if let Some(agent) = entry.agent_id.filter(|id| !id.is_empty()) {
        payload.insert("agent_id".into(), agent.into());
    }
    for (field, count) in tokens {
        if let Some(count) = count {
            payload.insert(field.into(), count.into());
        }
    }
    if registry
        .kind(KIND)
        .is_some_and(|spec| spec.fields.contains("project"))
    {
        let label = project.map_or("", |p| p.label.as_str());
        payload.insert("project".into(), label.into());
    }
    let Ok(mut env) = make_envelope(KIND, payload, registry, cfg.strict) else {
        return Line::Unreadable;
    };
    env.ts = occurred.to_string();
    env.metadata.source = Some(SOURCE.into());
    env.metadata.record_id = Some(crate::model::source_record_id(KIND, &[&request_id]));
    env.metadata.project = project.cloned();
    Line::Response(Box::new(env))
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// The file a position was measured in, so a transcript replaced under the same path is read from
/// its start instead of from an offset into different content. Where the platform names no file
/// identity (outside Unix), only a transcript shorter than its position is read again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileId {
    device: u64,
    inode: u64,
}

impl FileId {
    #[cfg(unix)]
    fn of(metadata: &std::fs::Metadata) -> Option<Self> {
        use std::os::unix::fs::MetadataExt;
        Some(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }

    #[cfg(not(unix))]
    fn of(_: &std::fs::Metadata) -> Option<Self> {
        None
    }
}

/// How far a transcript has been read: the byte after the last complete line recorded.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Position {
    transcript: PathBuf,
    file: Option<FileId>,
    offset: u64,
}

impl Position {
    fn path(positions: &Path, transcript: &Path) -> PathBuf {
        let name = blake3::hash(transcript.as_os_str().as_encoded_bytes()).to_hex();
        positions.join(format!("{name}.json"))
    }

    fn load(positions: &Path, transcript: &Path) -> Option<Self> {
        let text = std::fs::read(Self::path(positions, transcript)).ok()?;
        serde_json::from_slice::<Self>(&text)
            .ok()
            .filter(|p| p.transcript == transcript)
    }

    /// Store `offset` unless another read of the same file already got further. Written through a
    /// rename, so a reader never sees half a position.
    fn save(positions: &Path, transcript: &Path, file: Option<FileId>, offset: u64) {
        if Self::load(positions, transcript).is_some_and(|p| p.file == file && p.offset >= offset) {
            return;
        }
        let position = Self {
            transcript: transcript.to_path_buf(),
            file,
            offset,
        };
        let path = Self::path(positions, transcript);
        let staged = path.with_extension(format!("{}.tmp", std::process::id()));
        let written = std::fs::create_dir_all(positions)
            .and_then(|()| std::fs::write(&staged, serde_json::to_vec(&position)?))
            .and_then(|()| std::fs::rename(&staged, &path));
        if let Err(e) = written {
            // The responses are recorded; without the position the next read only repeats work.
            eprintln!("hatel hook: {}: {e}", path.display());
            let _ = std::fs::remove_file(&staged);
        }
    }
}

/// Remove what is kept for transcripts Claude Code no longer has: a position whose transcript is
/// gone or that cannot be read, and a session's subagent list none of whose transcripts remain.
/// Returns how many files were removed. Both outlive their session for as long as Claude Code keeps
/// the transcripts, since a resumed session appends to them.
pub fn prune_positions(state_dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(state_dir.join(POSITIONS)) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let stale = if path.extension().is_some_and(|e| e == SUBAGENTS) {
            std::fs::read_to_string(&path)
                .map_or(true, |list| !list.lines().any(|l| Path::new(l).exists()))
        } else {
            std::fs::read(&path)
                .ok()
                .and_then(|text| serde_json::from_slice::<Position>(&text).ok())
                .is_none_or(|p| !p.transcript.exists())
        };
        if stale && std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn config(dir: &Path) -> Config {
        Config {
            state_dir: dir.to_path_buf(),
            plugins: Vec::new(),
            plugin_source: crate::config::PluginSource::ConfigFile,
            rotate_bytes: 10 * 1024 * 1024,
            retention_days: 90,
            kind_retention_days: Default::default(),
            disabled: false,
            strict: true,
            export: Ok(Default::default()),
        }
    }

    fn assistant(session: &str, request: &str, output: u64) -> String {
        entry(
            session,
            request,
            serde_json::json!({"input_tokens": 3, "output_tokens": output,
                "cache_read_input_tokens": 100, "cache_creation_input_tokens": 10}),
        )
    }

    fn entry(session: &str, request: &str, usage: serde_json::Value) -> String {
        serde_json::json!({
            "type": "assistant", "sessionId": session, "requestId": request,
            "timestamp": "2026-11-06T07:00:00.000Z", "uuid": format!("u-{request}"),
            "message": {"id": format!("m-{request}"), "model": "claude-opus-5-5",
                "content": [{"type": "text", "text": "ok"}], "usage": usage}
        })
        .to_string()
            + "\n"
    }

    fn append(path: &Path, text: &str) {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap()
            .write_all(text.as_bytes())
            .unwrap();
    }

    fn read_once(cfg: &Config, registry: &Registry, transcript: &Path) -> Read {
        let mut sink = crate::build_sink(cfg, registry);
        read(cfg, registry, &mut sink, transcript, None).unwrap()
    }

    fn stored(cfg: &Config) -> Vec<crate::Envelope> {
        let conn = rusqlite::Connection::open(cfg.database()).unwrap();
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {} FROM envelopes e WHERE e.kind = ?1 ORDER BY e.id",
                crate::sink::ENVELOPE_COLUMNS
            ))
            .unwrap();
        stmt.query_map([KIND], crate::sink::envelope)
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    #[test]
    fn a_response_carries_its_request_usage_and_occurrence() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path());
        let registry = crate::schema::load_core().unwrap();
        let transcript = dir.path().join("s.jsonl");
        append(&transcript, &assistant("S", "req_1", 42));
        let mut sub =
            serde_json::from_str::<serde_json::Value>(&assistant("S", "req_2", 7)).unwrap();
        sub["agentId"] = "a1".into();
        sub["isSidechain"] = true.into();
        append(&transcript, &format!("{sub}\n"));

        assert_eq!(read_once(&cfg, &registry, &transcript).recorded, 2);
        let records = stored(&cfg);
        let main = &records[0].payload;
        assert_eq!(main["request_id"], "req_1");
        assert_eq!(main["output_tokens"], 42);
        assert_eq!(main["cache_read_tokens"], 100);
        assert_eq!(main["cache_creation_tokens"], 10);
        assert_eq!(main["model"], "claude-opus-5-5");
        assert!(
            !main.contains_key("agent_id"),
            "the main agent names no agent"
        );
        assert_eq!(records[1].payload["agent_id"], "a1");
        assert_eq!(records[0].ts, "2026-11-06T07:00:00Z");
        assert_eq!(records[0].metadata.source.as_deref(), Some(SOURCE));
    }

    #[test]
    fn each_read_records_only_complete_lines_it_has_not_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path());
        let registry = crate::schema::load_core().unwrap();
        let transcript = dir.path().join("s.jsonl");
        // One response written in several entries, one per content block, repeats its usage.
        append(&transcript, &assistant("S", "req_1", 42).repeat(2));
        let next = assistant("S", "req_2", 5);
        let (head, tail) = next.split_at(20);
        append(&transcript, head);

        assert_eq!(read_once(&cfg, &registry, &transcript).recorded, 1);
        append(&transcript, tail);
        assert_eq!(read_once(&cfg, &registry, &transcript).recorded, 1);
        assert_eq!(read_once(&cfg, &registry, &transcript), Read::default());
        assert_eq!(stored(&cfg).len(), 2);
    }

    #[test]
    fn a_replaced_transcript_is_read_from_its_start_without_repeating_a_request() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path());
        let registry = crate::schema::load_core().unwrap();
        let transcript = dir.path().join("s.jsonl");
        append(&transcript, &assistant("S", "req_1", 42));
        read_once(&cfg, &registry, &transcript);

        let replacement = dir.path().join("new.jsonl");
        append(&replacement, &assistant("S", "req_1", 42));
        append(&replacement, &assistant("S", "req_2", 1));
        std::fs::rename(&replacement, &transcript).unwrap();

        assert_eq!(read_once(&cfg, &registry, &transcript).recorded, 1);
        assert_eq!(stored(&cfg).len(), 2);
    }

    #[test]
    fn a_forked_copy_of_a_request_keeps_the_session_that_recorded_it() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path());
        let registry = crate::schema::load_core().unwrap();
        let parent = dir.path().join("parent.jsonl");
        let fork = dir.path().join("fork.jsonl");
        append(&parent, &assistant("P", "req_1", 42));
        append(&fork, &assistant("F", "req_1", 42));
        append(&fork, &assistant("F", "req_2", 4));

        read_once(&cfg, &registry, &parent);
        assert_eq!(read_once(&cfg, &registry, &fork).recorded, 1);
        let sessions: Vec<_> = stored(&cfg)
            .iter()
            .map(|r| r.payload["session_id"].clone())
            .collect();
        assert_eq!(sessions, ["P", "F"]);
    }

    #[test]
    fn an_inherited_copy_without_tokens_leaves_the_request_to_its_billed_entry() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path());
        let registry = crate::schema::load_core().unwrap();
        let inheritor = dir.path().join("agent-b.jsonl");
        let original = dir.path().join("agent-a.jsonl");
        let inherited = serde_json::json!({"input_tokens": 0, "output_tokens": 0,
            "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0});
        append(&inheritor, &entry("S", "req_1", inherited));
        append(&original, &assistant("S", "req_1", 42));

        assert_eq!(read_once(&cfg, &registry, &inheritor), Read::default());
        assert_eq!(read_once(&cfg, &registry, &original).recorded, 1);
        assert_eq!(stored(&cfg)[0].payload["output_tokens"], 42);
    }

    #[test]
    fn lines_that_are_not_responses_are_skipped_and_damaged_ones_counted() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path());
        let registry = crate::schema::load_core().unwrap();
        let transcript = dir.path().join("s.jsonl");
        let user = serde_json::json!({"type": "user", "sessionId": "S",
            "message": {"content": "a tool result quoting \"requestId\""}});
        let refused = serde_json::json!({"type": "assistant", "sessionId": "S", "requestId": "req_0",
            "timestamp": "2026-11-06T07:00:00Z",
            "message": {"model": "<synthetic>", "usage": {"input_tokens": 0, "output_tokens": 0}}});
        let undated = serde_json::json!({"type": "assistant", "sessionId": "S", "requestId": "req_9",
            "message": {"usage": {"output_tokens": 1}}});
        append(
            &transcript,
            &format!("{user}\n{refused}\n{undated}\n{{\"requestId\": broken\n"),
        );

        let outcome = read_once(&cfg, &registry, &transcript);
        assert_eq!(
            outcome,
            Read {
                recorded: 0,
                unreadable: 2
            }
        );
    }

    #[test]
    fn positions_of_removed_transcripts_are_pruned() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path());
        let registry = crate::schema::load_core().unwrap();
        let kept = dir.path().join("kept.jsonl");
        let gone = dir.path().join("gone.jsonl");
        append(&kept, &assistant("S", "req_1", 1));
        append(&gone, &assistant("T", "req_2", 1));
        read_once(&cfg, &registry, &kept);
        read_once(&cfg, &registry, &gone);
        let positions = dir.path().join(POSITIONS);
        remember_subagent(&positions, "S", &kept);
        remember_subagent(&positions, "T", &gone);
        std::fs::remove_file(&gone).unwrap();

        assert_eq!(
            prune_positions(dir.path()),
            2,
            "the position and the list naming only it"
        );
        assert!(Position::load(&positions, &kept).is_some());
        assert_eq!(subagents(&positions, "S"), [kept]);
    }
}
