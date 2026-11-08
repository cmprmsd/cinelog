//! `cinelog search --team` (work package M3-D; plan §7.7; docs/API.md §8).
//!
//! [`store`] gives a [`Store`] whose reader is a [`Searcher`] over the team server's HTTPS
//! API (`GET /api/search`, `/api/show`, `/api/sessions/{uuid}`, `/api/status`,
//! `/api/import`), authenticated with the token of `cinelog token team-login` and pinned to
//! its certificate ([`crate::token::TeamClient`]). It is read-only: `meta()` and `writer()`
//! fail. As root it is the team database itself (read directly, like a plain `search`).
//!
//! The API never returns paths, so a session's `cast_path` is its web URL; that is what
//! `search` prints on Enter when no local server is known.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use cinelog_core::Scope;
use cinelog_core::server::ServerInfo;
use cinelog_index::model::{
    self as im, ChunkKind, Cursor, EntryDetail, EntryRef, ExitFilter, HitKind, IndexStatus, LegacyImportRow,
    LegacyStatus, SearchHit, SearchPage, SearchQuery, SearchTarget, SessionState, SessionSummary,
};
use cinelog_index::{IndexError, IndexWriter, MetaStore, Searcher};
use cinelog_web::api::types as wire;

use super::{DbStore, Store};
use crate::ctx::Ctx;
use crate::token::{TeamClient, query_escape};

/// Largest page and output size the API serves.
const MAX_LIMIT: usize = 500;
const MAX_SHOW_BYTES: usize = 1 << 20;

pub fn store(ctx: &Ctx) -> anyhow::Result<Box<dyn Store>> {
    if ctx.scope() == Scope::Team {
        return Ok(Box::new(DbStore::new(ctx)));
    }
    Ok(Box::new(TeamStore {
        client: Arc::new(TeamClient::from_saved(ctx)?),
    }))
}

/// The team server as a `server.json`-like record, for result URLs (`--json`, Enter).
pub fn server(ctx: &Ctx) -> Option<ServerInfo> {
    if ctx.scope() == Scope::Team {
        return ServerInfo::read_live(&ctx.paths.runtime_dir);
    }
    let creds = crate::token::TeamCredentials::load(&crate::token::TeamCredentials::path(ctx)).ok()?;
    let url = crate::token::BaseUrl::parse(&creds.url).ok()?;
    Some(ServerInfo {
        pid: 0,
        url: url.to_string(),
        port: url.port,
        fingerprint: creds.fingerprint,
        version: String::new(),
    })
}

pub struct TeamStore {
    client: Arc<TeamClient>,
}

const READ_ONLY: &str = "the team history is read-only from member shells; forget runs on the team server (`sudo cinelog forget …`) or in its web UI";

impl Store for TeamStore {
    fn reader(&self) -> anyhow::Result<Box<dyn Searcher>> {
        Ok(Box::new(Remote::new(self.client.clone())))
    }

    fn meta(&self) -> anyhow::Result<Box<dyn MetaStore>> {
        anyhow::bail!(READ_ONLY)
    }

    fn writer(&self) -> anyhow::Result<Option<Box<dyn IndexWriter>>> {
        anyhow::bail!(READ_ONLY)
    }

    fn has_index(&self) -> bool {
        true
    }
}

/// The team server's index. The API's cursors are opaque strings; the model cursor handed
/// to the caller indexes into `cursors`.
pub struct Remote {
    client: Arc<TeamClient>,
    cursors: Mutex<Vec<String>>,
}

impl Remote {
    pub fn new(client: Arc<TeamClient>) -> Self {
        Self {
            client,
            cursors: Mutex::new(Vec::new()),
        }
    }

    fn get<T: serde::de::DeserializeOwned>(&self, target: &str) -> cinelog_index::Result<Option<T>> {
        self.client
            .get_json(target)
            .map_err(|e| IndexError::Io(std::io::Error::other(format!("{e:#}"))))
    }

    fn session_url(&self, uuid: &str) -> PathBuf {
        PathBuf::from(format!("{}/s/{uuid}", self.client.url))
    }

    fn query_target(&self, q: &SearchQuery) -> cinelog_index::Result<String> {
        let mut p: Vec<(&str, String)> = vec![("q", q.term.clone())];
        let target = match q.target {
            SearchTarget::Commands => "cmd",
            SearchTarget::Output => "out",
            SearchTarget::All => "both",
        };
        p.push(("in", target.into()));
        if q.case_sensitive {
            p.push(("case", "true".into()));
        }
        let f = &q.filters;
        let mut opt = |k: &'static str, v: &Option<String>| {
            if let Some(v) = v {
                p.push((k, v.clone()));
            }
        };
        opt("user", &f.user);
        opt("host", &f.host);
        opt("cwd", &f.cwd);
        opt("pane", &f.pane);
        opt("session", &f.session);
        if let Some(s) = f.since {
            p.push(("from", s.to_string()));
        }
        if let Some(u) = f.until {
            p.push(("to", u.to_string()));
        }
        if let Some(e) = f.exit {
            p.push((
                "exit",
                match e {
                    ExitFilter::Code(c) => c.to_string(),
                    ExitFilter::Success => "ok".into(),
                    ExitFilter::Failure => "fail".into(),
                },
            ));
        }
        if f.include_hidden {
            p.push(("hidden", "true".into()));
        }
        if !f.tags.is_empty() {
            p.push(("tag", f.tags.join(",")));
        }
        p.push(("limit", q.limit.clamp(1, MAX_LIMIT).to_string()));
        if let Some(c) = q.cursor {
            let cursors = self.cursors.lock().expect("cursor lock");
            let s = usize::try_from(c.id)
                .ok()
                .and_then(|i| cursors.get(i))
                .ok_or_else(|| IndexError::Invalid(format!("cursor {c}")))?;
            p.push(("cursor", s.clone()));
        }
        let query: Vec<String> = p.iter().map(|(k, v)| format!("{k}={}", query_escape(v))).collect();
        Ok(format!("/api/search?{}", query.join("&")))
    }
}

impl Searcher for Remote {
    fn search(&self, query: &SearchQuery) -> cinelog_index::Result<SearchPage> {
        let target = self.query_target(query)?;
        let resp: wire::SearchResponse = self
            .get(&target)?
            .ok_or_else(|| IndexError::NotFound("the team server has no search API".into()))?;
        let hits: Vec<SearchHit> = resp.hits.into_iter().map(hit).collect();
        let next = match (resp.next, hits.last()) {
            (Some(s), Some(last)) => {
                let mut cursors = self.cursors.lock().expect("cursor lock");
                cursors.push(s);
                Some(Cursor {
                    ts: last.ts,
                    kind: last.kind,
                    id: (cursors.len() - 1) as i64,
                })
            }
            _ => None,
        };
        Ok(SearchPage {
            hits,
            next,
            partial: resp.partial,
            capped: resp.capped,
        })
    }

    fn session(&self, uuid: &str) -> cinelog_index::Result<Option<SessionSummary>> {
        let uuid = valid_uuid(uuid)?;
        let detail: Option<wire::SessionDetail> = self.get(&format!("/api/sessions/{uuid}"))?;
        Ok(detail.map(|d| summary(d.session, self.session_url(&uuid))))
    }

    fn show(&self, entry: &EntryRef, max_output_bytes: usize) -> cinelog_index::Result<Option<EntryDetail>> {
        let max = max_output_bytes.clamp(1, MAX_SHOW_BYTES);
        let target = format!("/api/show?entry={}&max_bytes={max}", query_escape(&entry.to_string()));
        let e: Option<wire::Entry> = self.get(&target)?;
        Ok(e.map(|e| {
            let url = self.session_url(&e.session.uuid);
            EntryDetail {
                session: summary(e.session, url),
                command: e.command.map(command),
                output: e.output,
                output_truncated: e.output_truncated,
            }
        }))
    }

    fn status(&self) -> cinelog_index::Result<IndexStatus> {
        let s: wire::Status = self
            .get("/api/status")?
            .ok_or_else(|| IndexError::NotFound("status".into()))?;
        let info = [
            ("server_version", s.version.clone()),
            ("server_url", self.client.url.to_string()),
            ("host", s.host.clone()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect();
        Ok(IndexStatus {
            schema_version: s.schema_version,
            index_bytes: s.index_bytes,
            sessions: im::SessionCounts {
                live: s.sessions.live,
                ended: s.sessions.ended,
                crashed: s.sessions.crashed,
                degraded: s.sessions.degraded,
                error: s.sessions.error,
                legacy: s.sessions.legacy,
                hidden: s.sessions.hidden,
            },
            commands: s.commands,
            outputs: s.outputs,
            live: s
                .live
                .into_iter()
                .map(|l| im::LiveLag {
                    cast_path: self.session_url(&l.session),
                    uuid: l.session,
                    indexed_bytes: l.indexed_bytes,
                    file_bytes: l.file_bytes,
                    updated_at: l.updated_at,
                })
                .collect(),
            backlog_bytes: s.backlog_bytes,
            legacy: im::LegacyProgress {
                files: s.legacy.files,
                imported: s.legacy.imported,
                hidden: s.legacy.hidden,
                skipped: s.legacy.skipped,
                failed: s.legacy.failed,
                banners: s.legacy.banners,
                log_lines: s.legacy.log_lines,
                aligned: s.legacy.aligned,
            },
            last_error: s.last_error,
            info,
        })
    }

    fn legacy_report(&self) -> cinelog_index::Result<Vec<LegacyImportRow>> {
        let r: wire::ImportReport = self
            .get("/api/import")?
            .ok_or_else(|| IndexError::NotFound("import report".into()))?;
        Ok(r.files
            .into_iter()
            .map(|f| LegacyImportRow {
                path: f.file.into(),
                file: im::FileStamp::default(),
                sha256: String::new(),
                commands_log: None,
                status: match f.status {
                    wire::LegacyStatus::Imported => LegacyStatus::Imported,
                    wire::LegacyStatus::Hidden => LegacyStatus::Hidden,
                    wire::LegacyStatus::Skipped => LegacyStatus::Skipped,
                    wire::LegacyStatus::Failed => LegacyStatus::Failed,
                },
                session_uuid: f.session,
                banners: f.banners,
                log_lines: f.log_lines,
                aligned: f.aligned,
                mean_conf: f.mean_conf,
                error: f.error,
            })
            .collect())
    }
}

/// Only uuids go into request paths.
fn valid_uuid(s: &str) -> cinelog_index::Result<String> {
    uuid::Uuid::try_parse(s)
        .map(|u| u.hyphenated().to_string())
        .map_err(|_| IndexError::Invalid(format!("session {s:?} is not a uuid")))
}

fn hit(h: wire::SearchHit) -> SearchHit {
    let kind = match h.kind {
        wire::HitKind::Command => HitKind::Command,
        wire::HitKind::Output => HitKind::Output,
    };
    let id = h.id.split_once(':').and_then(|(_, n)| n.parse().ok()).unwrap_or(0);
    // Output hits carry the snippet; the sentinels go back in for the local renderer.
    let snippet = (kind == HitKind::Output).then(|| {
        let mut s = String::new();
        for part in &h.snippet {
            if part.hit {
                s.push(im::SNIPPET_OPEN);
                s.push_str(&part.text);
                s.push(im::SNIPPET_CLOSE);
            } else {
                s.push_str(&part.text);
            }
        }
        s
    });
    SearchHit {
        kind,
        id,
        session_uuid: h.session,
        cmd_seq: h.cmd_seq,
        ts: h.ts,
        t: h.t,
        user: h.user,
        host: h.host,
        exit: h.exit,
        cwd: h.cwd,
        typed: h.typed,
        snippet,
        chunk_kind: h.chunk_kind.map(|k| match k {
            wire::ChunkKind::Primary => ChunkKind::Primary,
            wire::ChunkKind::Alt => ChunkKind::Alt,
            wire::ChunkKind::Gap => ChunkKind::Gap,
            wire::ChunkKind::Provisional => ChunkKind::Provisional,
        }),
        approx: h.approx,
        confidence: h.confidence,
    }
}

fn summary(s: wire::Session, url: PathBuf) -> SessionSummary {
    SessionSummary {
        uuid: s.uuid,
        host: s.host,
        user: s.user,
        scope: match s.scope {
            wire::Scope::Team => Scope::Team,
            wire::Scope::Personal => Scope::Personal,
        },
        state: match s.state {
            wire::SessionState::Live => SessionState::Live,
            wire::SessionState::Ended => SessionState::Ended,
            wire::SessionState::Crashed => SessionState::Crashed,
            wire::SessionState::Degraded => SessionState::Degraded,
            wire::SessionState::Error => SessionState::Error,
        },
        started_at: s.started_at,
        ended_at: s.ended_at,
        cast_path: url,
        tmux: im::TmuxLocation {
            server: s.tmux.server,
            session_id: s.tmux.session_id,
            session: s.tmux.session,
            window: s.tmux.window,
            window_name: s.tmux.window_name,
            pane: s.tmux.pane,
        },
        legacy: s.legacy,
        hidden: s.hidden,
        commands: s.commands,
    }
}

fn command(c: wire::Command) -> im::Command {
    im::Command {
        seq: c.seq,
        parent_seq: c.parent_seq,
        depth: c.depth,
        shell_pid: None,
        t_start: c.t_start,
        t_end: c.t_end,
        ts_start: c.ts_start,
        ts_end: c.ts_end,
        exit: c.exit,
        pipestatus: c.pipestatus,
        cwd: c.cwd,
        user: c.user,
        euid: None,
        typed: c.typed,
        expanded: c.expanded,
        typed_trunc: c.typed_trunc,
        approx: c.approx,
        confidence: c.confidence,
        out_bytes: c.out_bytes,
        out_truncated: c.out_truncated,
    }
}

#[cfg(test)]
mod tests {
    use cinelog_index::model::SearchFilters;
    use clap::Parser as _;

    use super::*;
    use crate::cli::{Cli, Command};
    use crate::search::list;
    use crate::search::testutil::{T0, U1, U2, env};
    use crate::token::BaseUrl;
    use crate::token::testserver::{self, FakeTeam, Reply, Req, TestServer};

    fn session(uuid: &str) -> wire::Session {
        wire::Session {
            uuid: uuid.into(),
            host: "teamhost".into(),
            scope: wire::Scope::Team,
            user: "root".into(),
            euid: Some(0),
            sudo_user: Some("alice".into()),
            claimed_user: None,
            attribution: Some("ancestor".into()),
            login_via: None,
            tty: Some("/dev/pts/4".into()),
            holder_comm: Some("sudo".into()),
            tmux: wire::TmuxLocation {
                pane: Some("%7".into()),
                ..wire::TmuxLocation::default()
            },
            parent_uuid: None,
            parts: 1,
            compressed: false,
            cast_bytes: Some(4096),
            started_at: T0,
            ended_at: Some(T0 + 60.0),
            duration: Some(60.0),
            exit_status: Some(0),
            tz: Some("UTC".into()),
            cols: 80,
            rows: 24,
            term: Some("xterm-256color".into()),
            legacy: false,
            hidden: false,
            state: wire::SessionState::Ended,
            commands: 2,
        }
    }

    fn command(seq: u64, typed: &str) -> wire::Command {
        wire::Command {
            seq,
            parent_seq: None,
            depth: 0,
            t_start: 5.0,
            t_end: Some(6.0),
            ts_start: T0 + 5.0,
            ts_end: Some(T0 + 6.0),
            exit: Some(1),
            pipestatus: vec![1],
            cwd: Some("/etc".into()),
            user: Some("root".into()),
            typed: typed.into(),
            expanded: None,
            typed_trunc: false,
            approx: false,
            confidence: None,
            out_bytes: 12,
            out_truncated: false,
        }
    }

    fn hit(id: &str, kind: wire::HitKind, seq: Option<u64>, ts: f64, snippet: &[(&str, bool)]) -> wire::SearchHit {
        wire::SearchHit {
            id: id.into(),
            kind,
            session: U1.into(),
            cmd_seq: seq,
            t: ts - T0,
            ts,
            user: "root".into(),
            host: "teamhost".into(),
            pane: Some("%7".into()),
            tmux_session: None,
            exit: Some(1),
            cwd: Some("/etc".into()),
            typed: seq.map(|_| "grep needle /etc/hosts".into()),
            snippet: snippet
                .iter()
                .map(|(t, h)| wire::SnippetPart {
                    text: (*t).into(),
                    hit: *h,
                })
                .collect(),
            chunk_kind: (kind == wire::HitKind::Output).then_some(wire::ChunkKind::Primary),
            approx: false,
            confidence: None,
            legacy: false,
            tags: Vec::new(),
        }
    }

    fn routes(req: &Req) -> Option<Reply> {
        let q = req.query();
        let get = |k: &str| q.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        match req.path() {
            "/api/search" => {
                let page = match get("cursor") {
                    None => wire::SearchResponse {
                        hits: vec![
                            hit(
                                "o:9",
                                wire::HitKind::Output,
                                Some(2),
                                T0 + 30.0,
                                &[("the ", false), ("needle", true), (" is", false)],
                            ),
                            hit(
                                "c:4",
                                wire::HitKind::Command,
                                Some(2),
                                T0 + 20.0,
                                &[("grep ", false), ("needle", true)],
                            ),
                        ],
                        next: Some("opaque/page=2".into()),
                        partial: false,
                        capped: true,
                    },
                    Some("opaque/page=2") => wire::SearchResponse {
                        hits: vec![hit(
                            "c:3",
                            wire::HitKind::Command,
                            Some(1),
                            T0 + 10.0,
                            &[("needle", true)],
                        )],
                        next: None,
                        partial: false,
                        capped: false,
                    },
                    Some(other) => panic!("cursor {other}"),
                };
                Some(Reply::json(200, &page))
            }
            "/api/show" if get("entry") == Some(&format!("{U1}:2")) => Some(Reply::json(
                200,
                &wire::Entry {
                    session: session(U1),
                    command: Some(command(2, "grep needle /etc/hosts")),
                    output: format!("max {}", get("max_bytes").unwrap()),
                    output_truncated: true,
                },
            )),
            p if p == format!("/api/sessions/{U1}") => Some(Reply::json(
                200,
                &wire::SessionDetail {
                    session: session(U1),
                    parent: None,
                    children: Vec::new(),
                    markers: Vec::new(),
                    parts: Vec::new(),
                    forgotten: Vec::new(),
                    masked: Vec::new(),
                    play_mode: wire::PlayMode::Cast,
                },
            )),
            "/api/status" => Some(Reply::json(
                200,
                &wire::Status {
                    version: "0.12.0".into(),
                    protocol: 1,
                    scope: wire::Scope::Team,
                    host: "teamhost".into(),
                    server_started_at: T0,
                    port: 10000,
                    fingerprint: testserver::fingerprint(),
                    schema_version: 3,
                    index_bytes: 1 << 20,
                    sessions: wire::SessionCounts {
                        live: 1,
                        ended: 5,
                        ..wire::SessionCounts::default()
                    },
                    commands: 42,
                    outputs: 7,
                    live: vec![wire::LiveLag {
                        session: U2.into(),
                        indexed_bytes: 10,
                        file_bytes: Some(20),
                        updated_at: None,
                    }],
                    backlog_bytes: 10,
                    legacy: wire::LegacyProgress::default(),
                    last_error: None,
                },
            )),
            _ => None,
        }
    }

    struct Fixture {
        team: Arc<FakeTeam>,
        server: TestServer,
        store: TeamStore,
    }

    fn fixture() -> Fixture {
        let (token, _) = cinelog_web::tokens::generate();
        let team = FakeTeam::new(&token);
        let server = team.serve(routes);
        let url = BaseUrl {
            host: "127.0.0.1".into(),
            port: server.port,
        };
        let client = TeamClient::new(url, Some(testserver::fingerprint()), token, None).unwrap();
        Fixture {
            team,
            server,
            store: TeamStore {
                client: Arc::new(client),
            },
        }
    }

    fn search_args(argv: &[&str]) -> crate::cli::SearchArgs {
        let mut full = vec!["cinelog", "search", "--team"];
        full.extend_from_slice(argv);
        match Cli::try_parse_from(full).unwrap().command {
            Command::Search(a) => a,
            _ => unreachable!(),
        }
    }

    #[test]
    fn search_pages_through_the_servers_cursors() {
        let f = fixture();
        let reader = f.store.reader().unwrap();
        let mut query = SearchQuery::new("needle");
        query.target = SearchTarget::All;
        query.case_sensitive = true;
        query.filters = SearchFilters {
            user: Some("root".into()),
            since: Some(T0),
            exit: Some(ExitFilter::Failure),
            cwd: Some("/etc".into()),
            include_hidden: true,
            ..SearchFilters::default()
        };
        let all = list::collect(&*reader, query, 10).unwrap();
        let entries: Vec<String> = all.hits.iter().map(|h| h.entry().to_string()).collect();
        assert_eq!(entries, [format!("{U1}:2"), format!("{U1}:2"), format!("{U1}:1")]);
        assert!(all.capped);
        let out = &all.hits[0];
        assert_eq!((out.kind, out.id), (HitKind::Output, 9));
        assert_eq!(out.snippet.as_deref(), Some("the \u{e000}needle\u{e001} is"));
        assert_eq!(out.chunk_kind, Some(ChunkKind::Primary));
        assert_eq!(
            (all.hits[1].kind, all.hits[1].id, all.hits[1].snippet.as_deref()),
            (HitKind::Command, 4, None)
        );

        let reqs = f.team.requests.lock().unwrap();
        let searches: Vec<&Req> = reqs.iter().filter(|r| r.path() == "/api/search").collect();
        assert_eq!(searches.len(), 2);
        let q = searches[0].query();
        let get = |k: &str| q.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        assert_eq!(get("q").as_deref(), Some("needle"));
        assert_eq!(get("in").as_deref(), Some("both"));
        assert_eq!(get("case").as_deref(), Some("true"));
        assert_eq!(get("user").as_deref(), Some("root"));
        assert_eq!(get("from"), Some(T0.to_string()));
        assert_eq!(get("exit").as_deref(), Some("fail"));
        assert_eq!(get("cwd").as_deref(), Some("/etc"));
        assert_eq!(get("hidden").as_deref(), Some("true"));
        assert_eq!(get("limit").as_deref(), Some("10"));
        assert_eq!(get("cursor"), None);
        assert!(
            searches[1].target.contains("cursor=opaque%2Fpage%3D2"),
            "{}",
            searches[1].target
        );
        assert_eq!(f.team.logins(), 1, "one login for the whole search");
    }

    #[test]
    fn the_search_command_prints_team_hits() {
        let f = fixture();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let args = search_args(&["-a", "--print", "needle"]);
        let code = list::search(&args, &f.store, &env(), list::Ui::Plain, &mut out, &mut err).unwrap();
        assert_eq!(code, 0);
        let out = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3, "{out}");
        assert!(lines[0].starts_with(&format!("{U1}:2\t")), "{out}");
        assert!(
            lines[0].contains("root@teamhost") && lines[0].contains("needle"),
            "{out}"
        );
        assert!(String::from_utf8(err).unwrap().contains("more than 5000"));
    }

    #[test]
    fn show_and_session_come_from_the_server() {
        let f = fixture();
        let reader = f.store.reader().unwrap();
        let entry = EntryRef::Command {
            uuid: U1.into(),
            seq: 2,
        };
        let d = reader.show(&entry, 4096).unwrap().unwrap();
        assert_eq!(d.output, "max 4096");
        assert!(d.output_truncated);
        let cmd = d.command.unwrap();
        assert_eq!(
            (cmd.seq, cmd.typed.as_str(), cmd.exit),
            (2, "grep needle /etc/hosts", Some(1))
        );
        let url = format!("https://127.0.0.1:{}/s/{U1}", f.server.port);
        assert_eq!(d.session.cast_path, PathBuf::from(&url));
        assert_eq!((d.session.user.as_str(), d.session.scope), ("root", Scope::Team));
        assert_eq!(d.session.tmux.pane.as_deref(), Some("%7"));
        let big = reader.show(&entry, 10 << 20).unwrap().unwrap();
        assert_eq!(big.output, format!("max {MAX_SHOW_BYTES}"));
        let other = EntryRef::Command {
            uuid: U2.into(),
            seq: 1,
        };
        assert_eq!(reader.show(&other, 4096).unwrap(), None);

        let s = reader.session(&U1.to_uppercase()).unwrap().unwrap();
        assert_eq!((s.uuid.as_str(), s.state, s.commands), (U1, SessionState::Ended, 2));
        assert_eq!(s.cast_path, PathBuf::from(url));
        assert_eq!(reader.session(U2).unwrap(), None);
        assert!(matches!(reader.session("../etc"), Err(IndexError::Invalid(_))));
    }

    #[test]
    fn status_maps_and_errors_surface() {
        let f = fixture();
        let reader = f.store.reader().unwrap();
        let s = reader.status().unwrap();
        assert_eq!((s.schema_version, s.commands, s.sessions.ended), (3, 42, 5));
        assert_eq!(s.live[0].uuid, U2);
        assert_eq!(s.info["server_version"], "0.12.0");
        // /api/import is not routed here: a 404 becomes NotFound.
        assert!(matches!(reader.legacy_report(), Err(IndexError::NotFound(_))));
        // A freed port can be re-bound at once by a parallel test's server, so point a second
        // client at a port that stays reserved but never listens: connects are refused.
        let (_reserved, port) = refusing_port();
        let url = BaseUrl {
            host: "127.0.0.1".into(),
            port,
        };
        let client = TeamClient::new(url, Some(testserver::fingerprint()), f.team.token.clone(), None).unwrap();
        let down = TeamStore {
            client: Arc::new(client),
        };
        let err = down
            .reader()
            .unwrap()
            .search(&SearchQuery::new("x"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("cannot reach the team server"), "{err}");
    }

    /// A loopback TCP port that is bound (so nobody else gets it) but not listening.
    fn refusing_port() -> (std::os::fd::OwnedFd, u16) {
        use std::os::fd::FromRawFd as _;
        // SAFETY: plain socket calls on a fresh fd that the OwnedFd owns from here on.
        unsafe {
            let fd = libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
            assert!(fd >= 0, "socket: {}", std::io::Error::last_os_error());
            let owned = std::os::fd::OwnedFd::from_raw_fd(fd);
            let mut addr: libc::sockaddr_in = std::mem::zeroed();
            addr.sin_family = libc::AF_INET as libc::sa_family_t;
            addr.sin_addr.s_addr = u32::from(std::net::Ipv4Addr::LOCALHOST).to_be();
            let len = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
            let rc = libc::bind(fd, (&raw const addr).cast(), len);
            assert_eq!(rc, 0, "bind: {}", std::io::Error::last_os_error());
            let mut got_len = len;
            let rc = libc::getsockname(fd, (&raw mut addr).cast(), &mut got_len);
            assert_eq!(rc, 0, "getsockname: {}", std::io::Error::last_os_error());
            (owned, u16::from_be(addr.sin_port))
        }
    }

    #[test]
    fn the_team_store_is_read_only() {
        let f = fixture();
        assert!(f.store.has_index());
        assert!(f.store.meta().is_err());
        assert!(f.store.writer().err().unwrap().to_string().contains("read-only"));
    }

    #[test]
    fn root_reads_the_team_database_directly() {
        use cinelog_core::paths::{EnvSnapshot, SystemRoot};
        let dir = tempfile::tempdir().unwrap();
        let root_ctx = Ctx::with(
            EnvSnapshot {
                uid: 0,
                euid: 0,
                user: "root".into(),
                home: "/root".into(),
                xdg_config_home: None,
                xdg_state_home: None,
                xdg_runtime_dir: None,
            },
            SystemRoot::prefixed(dir.path()),
        );
        let s = store(&root_ctx).unwrap();
        assert!(!s.has_index(), "the (empty) team DB, not the API");
        let err = s.reader().err().unwrap().to_string();
        assert!(
            err.contains(&dir.path().join("var/lib/cinelog").display().to_string()),
            "{err}"
        );

        let user_ctx = Ctx::with(
            EnvSnapshot {
                uid: 1000,
                euid: 1000,
                user: "alice".into(),
                home: dir.path().join("home"),
                xdg_config_home: Some(dir.path().join("cfg")),
                xdg_state_home: None,
                xdg_runtime_dir: Some(dir.path().join("run")),
            },
            SystemRoot::real(),
        );
        let err = store(&user_ctx).err().unwrap().to_string();
        assert!(err.contains("team-login"), "{err}");
        assert_eq!(server(&user_ctx), None);
        crate::token::TeamCredentials {
            url: "https://team.lan:10000".into(),
            token: "T".into(),
            fingerprint: testserver::fingerprint(),
        }
        .save(&crate::token::TeamCredentials::path(&user_ctx))
        .unwrap();
        let info = server(&user_ctx).unwrap();
        assert_eq!(info.session_url(U1), format!("https://team.lan:10000/s/{U1}"));
    }

    /// Picks the first line it is shown.
    struct FirstLine(std::rc::Rc<std::cell::RefCell<Vec<String>>>);

    impl crate::search::pick::Pick for FirstLine {
        fn start(&self) -> std::io::Result<Box<dyn crate::search::pick::Picking>> {
            Ok(Box::new(FirstLine(self.0.clone())))
        }
    }

    impl crate::search::pick::Picking for FirstLine {
        fn feed(&mut self, lines: &[String]) -> std::io::Result<bool> {
            self.0.borrow_mut().extend_from_slice(lines);
            Ok(true)
        }
        fn finish(self: Box<Self>) -> std::io::Result<crate::search::pick::Picked> {
            Ok(crate::search::pick::Picked::Line(self.0.borrow()[0].clone()))
        }
    }

    /// Saves team-login credentials for the fixture's server in a member context.
    fn member_ctx(dir: &std::path::Path, f: &Fixture) -> Ctx {
        use cinelog_core::paths::{EnvSnapshot, SystemRoot};
        let ctx = Ctx::with(
            EnvSnapshot {
                uid: 1000,
                euid: 1000,
                user: "alice".into(),
                home: dir.join("home"),
                xdg_config_home: Some(dir.join("cfg")),
                xdg_state_home: Some(dir.join("state")),
                xdg_runtime_dir: Some(dir.join("run")),
            },
            SystemRoot::real(),
        );
        crate::token::TeamCredentials {
            url: format!("https://127.0.0.1:{}", f.server.port),
            token: f.team.token.clone(),
            fingerprint: testserver::fingerprint(),
        }
        .save(&crate::token::TeamCredentials::path(&ctx))
        .unwrap();
        ctx
    }

    /// `--json` and Enter link team hits to the team server, also while the member's own
    /// personal server runs (its `server.json` is what `Env` carries by default).
    #[test]
    fn team_hits_link_to_the_team_server() {
        let f = fixture();
        let dir = tempfile::tempdir().unwrap();
        let ctx = member_ctx(dir.path(), &f);
        let team_url = format!("https://127.0.0.1:{}/s/{U1}?cmd=2", f.server.port);
        let mut env = env();
        env.server = Some(ServerInfo {
            pid: 1,
            url: "https://box:11000".into(),
            port: 11000,
            fingerprint: String::new(),
            version: String::new(),
        });
        env.server = server(&ctx);
        assert_eq!(env.server.as_ref().map(|s| s.port), Some(f.server.port));

        let (mut out, mut err) = (Vec::new(), Vec::new());
        let args = search_args(&["--json", "needle"]);
        list::search(&args, &f.store, &env, list::Ui::Json, &mut out, &mut err).unwrap();
        let first: serde_json::Value =
            serde_json::from_str(String::from_utf8(out).unwrap().lines().next().unwrap()).unwrap();
        assert_eq!(first["url"], team_url.as_str());

        let picker = FirstLine(Default::default());
        let mut out = Vec::new();
        let code = list::search(
            &search_args(&["needle"]),
            &f.store,
            &env,
            list::Ui::Pick(&picker),
            &mut out,
            &mut err,
        )
        .unwrap();
        assert_eq!((code, String::from_utf8(out).unwrap()), (0, format!("{team_url}\n")));
    }

    /// The fzf preview of a team hit (`cinelog show <id>` on the team store) renders the
    /// server's `/api/show` answer.
    #[test]
    fn the_preview_shows_team_entries() {
        let f = fixture();
        let args = match Cli::try_parse_from(["cinelog", "show", &format!("{U1}:2")])
            .unwrap()
            .command
        {
            Command::Show(a) => a,
            _ => unreachable!(),
        };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = super::super::show::show(&args, &f.store, &env(), &mut out, &mut err).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert_eq!(code, 0, "{}", String::from_utf8_lossy(&err));
        assert!(out.contains("grep needle /etc/hosts") && out.contains("max "), "{out}");
    }
}
