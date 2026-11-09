//! `cinelog search` (§9.5): query building, paging, the tab-separated result lines and the
//! action on Enter.

use std::io::Write;
use std::path::{Component, Path, PathBuf};

use anyhow::Context as _;
use cinelog_core::server::ServerInfo;
use cinelog_index::Searcher;
use cinelog_index::model::{
    ChunkKind, EntryRef, ExitFilter, HitKind, SearchFilters, SearchHit, SearchQuery, SearchTarget,
};

use super::pick::{Pick, Picked};
use super::text::{self, DIM, RED, Style};
use super::when::{self, Bound};
use super::{Env, Store, code};
use crate::cli::SearchArgs;

/// The first page is small so the first results show at once (§13 M2: < 300 ms); later
/// pages fill `--limit` while they are already on screen.
const FIRST_PAGE: usize = 100;
const PAGE: usize = 500;
/// Display caps of the result line (fzf matches on what it shows).
const TYPED_CHARS: usize = 300;
const SNIPPET_CHARS: usize = 200;
const TAGS_CHARS: usize = 100;

/// How results are presented.
pub enum Ui<'a> {
    /// fzf, with `cinelog show {1}` as preview.
    Pick(&'a dyn Pick),
    /// The same lines, printed.
    Plain,
    /// One JSON object per hit.
    Json,
}

pub fn search(
    args: &SearchArgs,
    store: &dyn Store,
    env: &Env,
    ui: Ui<'_>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> anyhow::Result<u8> {
    let query = build_query(args, env)?;
    let reader = store.reader()?;
    let mut pages = Pages::new(&*reader, query, args.limit);
    let Some(first) = pages.next_page()? else {
        pages.warn(err)?;
        writeln!(err, "cinelog: no matches")?;
        return Ok(code::NONE);
    };
    // Before fzf takes the terminal; flags raised by later pages are reported after it.
    pages.warn(err)?;
    let code = match ui {
        Ui::Json | Ui::Plain => {
            let json = matches!(ui, Ui::Json);
            let mut page = Some(first);
            while let Some(hits) = page {
                for hit in &hits {
                    if json {
                        let url = env.server.as_ref().map(|s| entry_url(s, &hit.entry()));
                        writeln!(out, "{}", hit_json(hit, url))?;
                    } else {
                        writeln!(out, "{}", hit_line(hit, env, env.style))?;
                    }
                }
                out.flush()?;
                page = pages.next_page()?;
            }
            code::OK
        }
        Ui::Pick(picker) => {
            let style = Style { color: !no_color() };
            let mut picking = picker.start().context("fzf")?;
            let mut page = Some(first);
            while let Some(hits) = page {
                let lines: Vec<String> = hits.iter().map(|h| hit_line(h, env, style)).collect();
                if !picking.feed(&lines).context("fzf")? {
                    break;
                }
                page = pages.next_page()?;
            }
            match picking.finish().context("fzf")? {
                Picked::Line(line) => enter(&line, args.team, &*reader, env, out)?,
                // Contract §6.1: 0 or 1; leaving the picker selects nothing.
                Picked::NoMatch | Picked::Aborted => code::NONE,
            }
        }
    };
    pages.warn(err)?;
    Ok(code)
}

fn no_color() -> bool {
    std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty())
}

/// The [`SearchQuery`] for `args` (paging fields are filled in by [`collect`]).
pub fn build_query(args: &SearchArgs, env: &Env) -> anyhow::Result<SearchQuery> {
    let bound = |v: &Option<String>, b: Bound, flag: &str| -> anyhow::Result<Option<f64>> {
        v.as_deref()
            .map(|s| when::parse(s, b, env.now, &env.tz).map_err(|e| anyhow::anyhow!("{flag} {e}")))
            .transpose()
    };
    let since = bound(&args.since, Bound::Since, "--since")?;
    let until = bound(&args.until, Bound::Until, "--until")?;
    if let (Some(s), Some(u)) = (since, until) {
        anyhow::ensure!(s <= u, "--since is after --until");
    }
    let exit = args
        .exit
        .as_deref()
        .map(str::parse::<ExitFilter>)
        .transpose()
        .context("--exit")?;
    let cwd = args
        .cwd
        .as_deref()
        .map(|c| resolve_cwd(c, env.pwd.as_deref(), &env.home))
        .transpose()?;
    let mut q = SearchQuery::new(args.term.join(" "));
    q.target = if args.all {
        SearchTarget::All
    } else if args.out {
        SearchTarget::Output
    } else {
        SearchTarget::Commands
    };
    anyhow::ensure!(
        q.target != SearchTarget::Output || !q.term.is_empty(),
        "--out needs a search term"
    );
    q.case_sensitive = args.case_sensitive;
    q.filters = SearchFilters {
        user: args.user.clone(),
        host: args.host.clone(),
        since,
        until,
        exit,
        cwd,
        pane: args.pane.clone(),
        session: None,
        include_hidden: false,
        tags: cinelog_web::api::types::normalize_tags(&args.tag).map_err(|e| anyhow::anyhow!("--tag: {e}"))?,
    };
    Ok(q)
}

/// `--cwd`: `.`, relative and `~` paths against the logical working directory and home,
/// normalised lexically like zsh's `$PWD` (no symlink resolution).
pub fn resolve_cwd(arg: &str, pwd: Option<&Path>, home: &Path) -> anyhow::Result<String> {
    let path = if arg == "~" {
        home.to_owned()
    } else if let Some(rest) = arg.strip_prefix("~/") {
        home.join(rest)
    } else if Path::new(arg).is_absolute() {
        PathBuf::from(arg)
    } else {
        pwd.context("--cwd: the current directory is unknown")?.join(arg)
    };
    let mut norm = PathBuf::from("/");
    for c in path.components() {
        match c {
            Component::Normal(p) => norm.push(p),
            Component::ParentDir => {
                norm.pop();
            }
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
        }
    }
    norm.into_os_string()
        .into_string()
        .map_err(|p| anyhow::anyhow!("--cwd {}: not valid UTF-8", PathBuf::from(p).display()))
}

/// Up to `limit` hits along the `(ts, id)` cursor, one page at a time.
pub struct Pages<'a> {
    searcher: &'a dyn Searcher,
    query: SearchQuery,
    left: usize,
    done: bool,
    partial: bool,
    capped: bool,
    /// The flags already reported by [`Pages::warn`].
    warned: (bool, bool),
}

impl<'a> Pages<'a> {
    pub fn new(searcher: &'a dyn Searcher, query: SearchQuery, limit: usize) -> Self {
        Self {
            searcher,
            query,
            left: limit.max(1),
            done: false,
            partial: false,
            capped: false,
            warned: (false, false),
        }
    }

    /// The next non-empty page; `None` at the end, at `limit`, or after a partial page.
    pub fn next_page(&mut self) -> anyhow::Result<Option<Vec<SearchHit>>> {
        if self.done || self.left == 0 {
            return Ok(None);
        }
        // A short term is one time-capped scan (§6.5): ask for everything at once.
        self.query.limit = if self.query.is_short() {
            self.left
        } else if self.query.cursor.is_none() {
            self.left.min(FIRST_PAGE)
        } else {
            self.left.min(PAGE)
        };
        let page = self.searcher.search(&self.query)?;
        self.partial |= page.partial;
        self.capped |= page.capped;
        let mut hits = page.hits;
        hits.truncate(self.left);
        self.left -= hits.len();
        // After a partial page another request would start a fresh scan and so defeat its
        // time limit.
        match page.next {
            Some(c) if !hits.is_empty() && !page.partial => self.query.cursor = Some(c),
            _ => self.done = true,
        }
        Ok((!hits.is_empty()).then_some(hits))
    }

    /// Reports the `partial` and `capped` flags seen so far, each once.
    pub fn warn(&mut self, err: &mut dyn Write) -> std::io::Result<()> {
        if self.partial && !self.warned.0 {
            writeln!(
                err,
                "cinelog: terms under 3 characters scan newest first for 500 ms: the result may be partial"
            )?;
        }
        if self.capped && !self.warned.1 {
            writeln!(
                err,
                "cinelog: more than 5000 matches, showing the newest; refine the query"
            )?;
        }
        self.warned = (self.partial, self.capped);
        Ok(())
    }
}

/// All pages of `query` up to `limit`, with the flags of the last request.
#[cfg(test)]
pub fn collect(
    searcher: &dyn Searcher,
    query: SearchQuery,
    limit: usize,
) -> anyhow::Result<cinelog_index::model::SearchPage> {
    let mut pages = Pages::new(searcher, query, limit);
    let mut all = cinelog_index::model::SearchPage::default();
    while let Some(hits) = pages.next_page()? {
        all.hits.extend(hits);
    }
    all.partial = pages.partial;
    all.capped = pages.capped;
    Ok(all)
}

/// `<entry>\t<ts>\t<user>@<host>\t[exit]\t<typed>[ [tag,…]][\t┊ <snippet>]` (§9.5), the
/// tags of the command's annotations only when it has any. The hidden first field is the
/// [`EntryRef`] that `cinelog show` takes.
pub fn hit_line(hit: &SearchHit, env: &Env, style: Style) -> String {
    let ts = style.paint(DIM, &text::fmt_ts(hit.ts, &env.tz));
    let who = format!("{}@{}", text::one_line(&hit.user, 64), text::one_line(&hit.host, 64));
    let exit = match hit.exit {
        Some(0) => "[0]".to_owned(),
        Some(e) => style.paint(RED, &format!("[{e}]")),
        None => "[-]".to_owned(),
    };
    let approx = if hit.approx { "≈ " } else { "" };
    let typed = match &hit.typed {
        Some(t) => format!("{approx}{}", text::one_line(t, TYPED_CHARS)),
        None => style.paint(DIM, "(outside commands)"),
    };
    let mut line = format!("{}\t{ts}\t{who}\t{exit}\t{typed}", hit.entry());
    if !hit.tags.is_empty() {
        let tags = text::one_line(&hit.tags.join(","), TAGS_CHARS);
        line.push(' ');
        line.push_str(&style.paint(DIM, &format!("[{tags}]")));
    }
    if hit.kind == HitKind::Output {
        line.push_str("\t┊ ");
        match hit.chunk_kind {
            Some(ChunkKind::Alt) => line.push_str(&style.paint(DIM, "[screen] ")),
            Some(ChunkKind::Provisional) => line.push_str(&style.paint(DIM, "[running] ")),
            _ => {}
        }
        line.push_str(&text::snippet(
            hit.snippet.as_deref().unwrap_or(""),
            SNIPPET_CHARS,
            style,
        ));
    }
    line
}

/// A hit as JSON: the [`SearchHit`] fields (`tags` always, empty without annotations) plus
/// `entry` (and `url` once a server runs).
pub fn hit_json(hit: &SearchHit, url: Option<String>) -> serde_json::Value {
    let mut v = serde_json::to_value(hit).unwrap_or_default();
    if let Some(obj) = v.as_object_mut() {
        obj.insert("entry".into(), hit.entry().to_string().into());
        if let Some(url) = url {
            obj.insert("url".into(), url.into());
        }
    }
    v
}

/// Web URL of an entry: `https://<host>:<port>/s/<uuid>?cmd=<seq>` (or `?t=<secs>`).
pub fn entry_url(server: &ServerInfo, entry: &EntryRef) -> String {
    let base = server.session_url(entry.uuid());
    match entry {
        EntryRef::Session { .. } => base,
        EntryRef::Command { seq, .. } => format!("{base}?cmd={seq}"),
        EntryRef::At { t, .. } => format!("{base}?t={t}"),
    }
}

/// Enter in fzf: the web URL (opened in a local graphical session), or before the web server
/// exists, the cast path and the `cinelog show` command (only `show --team` for a team hit,
/// whose cast is on the team server).
fn enter(line: &str, team: bool, reader: &dyn Searcher, env: &Env, out: &mut dyn Write) -> anyhow::Result<u8> {
    let id = line.split('\t').next().unwrap_or_default();
    let entry: EntryRef = id.parse()?;
    if let Some(server) = &env.server {
        let url = entry_url(server, &entry);
        writeln!(out, "{url}")?;
        if env.can_open {
            open_url(&url);
        }
        return Ok(code::OK);
    }
    if team {
        writeln!(out, "cinelog show --team {entry}")?;
        return Ok(code::OK);
    }
    let session = reader
        .session(entry.uuid())?
        .with_context(|| format!("session {} is no longer in the index", entry.uuid()))?;
    writeln!(out, "{}", session.cast_path.display())?;
    writeln!(out, "cinelog show {entry}")?;
    Ok(code::OK)
}

fn open_url(url: &str) {
    use std::process::{Command, Stdio};
    // Not waited for: a browser started by xdg-open may run for hours.
    let spawned = Command::new("xdg-open")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if let Err(e) = spawned {
        eprintln!("cinelog: xdg-open: {e}");
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use clap::Parser;

    use super::super::pick::Picking;
    use super::super::testutil::*;
    use super::*;
    use crate::cli::{Cli, Command};

    fn args(argv: &[&str]) -> SearchArgs {
        let mut full = vec!["cinelog", "search"];
        full.extend_from_slice(argv);
        match Cli::try_parse_from(full).unwrap().command {
            Command::Search(a) => a,
            _ => unreachable!(),
        }
    }

    fn run(argv: &[&str], ui: Ui<'_>) -> (u8, String, String) {
        let store = store();
        capture(|out, err| search(&args(argv), &store, &env(), ui, out, err))
    }

    /// Picks the line whose typed field contains `want`, recording what it was shown.
    #[derive(Default)]
    struct FakePicker {
        want: Option<&'static str>,
        shown: Rc<RefCell<Vec<Vec<String>>>>,
        /// Ends the pick (as if a line was chosen) after this many batches.
        stop_after: Option<usize>,
    }

    struct FakePicking {
        want: Option<&'static str>,
        shown: Rc<RefCell<Vec<Vec<String>>>>,
        stop_after: Option<usize>,
    }

    impl Pick for FakePicker {
        fn start(&self) -> std::io::Result<Box<dyn Picking>> {
            Ok(Box::new(FakePicking {
                want: self.want,
                shown: self.shown.clone(),
                stop_after: self.stop_after,
            }))
        }
    }

    impl Picking for FakePicking {
        fn feed(&mut self, lines: &[String]) -> std::io::Result<bool> {
            let mut shown = self.shown.borrow_mut();
            shown.push(lines.to_vec());
            Ok(self.stop_after.is_none_or(|n| shown.len() < n))
        }
        fn finish(self: Box<Self>) -> std::io::Result<Picked> {
            let shown = self.shown.borrow();
            Ok(match self.want {
                Some(w) => shown
                    .iter()
                    .flatten()
                    .find(|l| l.contains(w))
                    .map_or(Picked::NoMatch, |l| Picked::Line(l.clone())),
                None => Picked::Aborted,
            })
        }
    }

    /// Serves `total` synthetic hits newest first, recording each request's limit.
    #[derive(Clone)]
    struct Stub {
        total: usize,
        /// `partial` on every page (a short-term scan that ran out of time).
        partial: bool,
        capped: bool,
        /// Shared by the clones handed out as readers.
        limits: std::sync::Arc<std::sync::Mutex<Vec<usize>>>,
    }

    impl Stub {
        fn new(total: usize) -> Self {
            Self {
                total,
                partial: false,
                capped: false,
                limits: Default::default(),
            }
        }
    }

    impl Searcher for Stub {
        fn search(&self, q: &SearchQuery) -> cinelog_index::Result<cinelog_index::model::SearchPage> {
            use cinelog_index::model::{Cursor, SearchPage};
            self.limits.lock().unwrap().push(q.limit);
            let start = q.cursor.map_or(0, |c| c.id as usize + 1);
            let end = (start + q.limit).min(self.total);
            let hits: Vec<SearchHit> = (start..end)
                .map(|i| SearchHit {
                    kind: HitKind::Command,
                    id: i as i64,
                    session_uuid: U1.into(),
                    cmd_seq: Some(i as u64 + 1),
                    ts: T0 - i as f64,
                    t: i as f64,
                    user: "alice".into(),
                    host: "box".into(),
                    exit: Some(0),
                    cwd: None,
                    typed: Some(format!("cmd {i}")),
                    snippet: None,
                    chunk_kind: None,
                    approx: false,
                    confidence: None,
                    tags: Vec::new(),
                })
                .collect();
            let next = (end < self.total || self.partial).then(|| Cursor {
                ts: T0 - end as f64 + 1.0,
                kind: HitKind::Command,
                id: end as i64 - 1,
            });
            Ok(SearchPage {
                hits,
                next,
                partial: self.partial,
                capped: self.capped,
            })
        }
        fn session(&self, u: &str) -> cinelog_index::Result<Option<cinelog_index::model::SessionSummary>> {
            use cinelog_index::model::{SessionState, SessionSummary};
            Ok(Some(SessionSummary {
                uuid: u.into(),
                host: "box".into(),
                user: "alice".into(),
                scope: cinelog_proto::Scope::Personal,
                state: SessionState::Live,
                started_at: T0,
                ended_at: None,
                cast_path: "/stub.cast".into(),
                tmux: Default::default(),
                legacy: false,
                hidden: false,
                commands: self.total as u64,
            }))
        }
        fn show(&self, _: &EntryRef, _: usize) -> cinelog_index::Result<Option<cinelog_index::model::EntryDetail>> {
            Ok(None)
        }
        fn status(&self) -> cinelog_index::Result<cinelog_index::model::IndexStatus> {
            unreachable!()
        }
        fn legacy_report(&self) -> cinelog_index::Result<Vec<cinelog_index::model::LegacyImportRow>> {
            unreachable!()
        }
    }

    /// A [`Store`] whose readers are clones of one [`Stub`].
    struct StubStore(Stub);

    impl Store for StubStore {
        fn reader(&self) -> anyhow::Result<Box<dyn Searcher>> {
            Ok(Box::new(self.0.clone()))
        }
        fn meta(&self) -> anyhow::Result<Box<dyn cinelog_index::MetaStore>> {
            unreachable!()
        }
        fn writer(&self) -> anyhow::Result<Option<Box<dyn cinelog_index::IndexWriter>>> {
            unreachable!()
        }
        fn has_index(&self) -> bool {
            true
        }
    }

    fn run_stub(stub: Stub, argv: &[&str], ui: Ui<'_>) -> (u8, String, String, Vec<usize>) {
        let limits = stub.limits.clone();
        let store = StubStore(stub);
        let (c, out, err) = capture(|o, e| search(&args(argv), &store, &env(), ui, o, e));
        let limits = limits.lock().unwrap().clone();
        (c, out, err, limits)
    }

    #[test]
    fn a_partial_page_ends_the_search() {
        let stub = Stub {
            partial: true,
            ..Stub::new(10_000)
        };
        let (c, out, err, limits) = run_stub(stub, &["ab"], Ui::Plain);
        assert_eq!(c, 0);
        assert_eq!(limits, [1000], "one scan, asking for the whole --limit");
        assert_eq!(out.lines().count(), 1000);
        assert_eq!(err.matches("may be partial").count(), 1, "{err}");
        // A long term with a partial page (not expected, but bounded the same way).
        let stub = Stub {
            partial: true,
            ..Stub::new(10_000)
        };
        let (_, _, err, limits) = run_stub(stub, &["abc"], Ui::Plain);
        assert_eq!(limits, [100]);
        assert!(err.contains("may be partial"), "{err}");
    }

    #[test]
    fn pages_grow_after_a_small_first_page() {
        let (c, out, err, limits) = run_stub(Stub::new(1234), &["needle", "--limit", "1200"], Ui::Plain);
        assert_eq!(c, 0);
        assert_eq!(limits, [100, 500, 500, 100]);
        assert_eq!(out.lines().count(), 1200);
        assert!(err.is_empty(), "{err}");
        let (_, out, _, limits) = run_stub(Stub::new(30), &["needle"], Ui::Plain);
        assert_eq!(limits, [100]);
        assert_eq!(out.lines().count(), 30);
    }

    #[test]
    fn the_picker_gets_each_page_as_it_arrives() {
        let picker = FakePicker::default();
        let (c, _, _, limits) = run_stub(Stub::new(700), &["needle"], Ui::Pick(&picker));
        assert_eq!(c, 1, "aborted: nothing selected");
        let sizes: Vec<usize> = picker.shown.borrow().iter().map(Vec::len).collect();
        assert_eq!(sizes, [100, 500, 100]);
        assert_eq!(limits, [100, 500, 400], "the default --limit is 1000");
        // Once fzf stops reading (a line was chosen), no further page is requested.
        let picker = FakePicker {
            want: Some("cmd 3"),
            stop_after: Some(1),
            ..FakePicker::default()
        };
        let (c, out, _, limits) = run_stub(Stub::new(5000), &["needle"], Ui::Pick(&picker));
        assert_eq!(c, 0);
        assert_eq!(out, format!("/stub.cast\ncinelog show {U1}:4\n"));
        assert_eq!(limits, [100]);
    }

    #[test]
    fn capped_results_warn_once_and_keep_paging() {
        let stub = Stub {
            capped: true,
            ..Stub::new(800)
        };
        let (c, out, err, limits) = run_stub(stub, &["needle"], Ui::Json);
        assert_eq!(c, 0);
        assert_eq!(limits, [100, 500, 400]);
        assert_eq!(out.lines().count(), 800);
        assert_eq!(err.matches("more than 5000 matches").count(), 1, "{err}");
    }

    #[test]
    fn command_search_prints_tab_separated_lines_newest_first() {
        let (c, out, _) = run(&["needle"], Ui::Plain);
        assert_eq!(c, 0);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(
            lines,
            [
                format!("{U2}:1\t2026-11-08 12:01:41\tbob@box\t[0]\tcat needle.log"),
                format!("{U1}:2\t2026-11-08 12:00:10\talice@box\t[1]\tgrep needle notes.txt"),
            ]
        );
    }

    #[test]
    fn all_includes_output_hits_with_safe_snippets() {
        let (c, out, _) = run(&["-a", "needle", "--user", "alice"], Ui::Plain);
        assert_eq!(c, 0);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2, "{out}");
        let output_hit = lines.iter().find(|l| l.contains('┊')).unwrap();
        assert!(output_hit.starts_with(&format!("{U1}:2\t")), "{output_hit}");
        assert!(
            output_hit.ends_with("\t┊ the needle is ^[[31mhere^[[0m"),
            "{output_hit}"
        );
        assert!(
            !out.contains('\x1b'),
            "no raw escape from the cast reaches the terminal"
        );
        assert_eq!(output_hit.split('\t').count(), 6);
    }

    #[test]
    fn gap_hits_address_a_point_in_time() {
        let (c, out, _) = run(&["--out", "box ~/src"], Ui::Plain);
        assert_eq!(c, 0);
        assert!(out.starts_with(&format!("{U1}@15\t")), "{out}");
        assert!(out.contains("(outside commands)"));
    }

    #[test]
    fn filters_reach_the_query() {
        let (c, out, _) = run(&["--exit", "fail"], Ui::Plain);
        assert_eq!(c, 0);
        assert_eq!(out.lines().count(), 1);
        assert!(out.contains("grep needle"));
        let (_, out, _) = run(&["--cwd", "."], Ui::Plain);
        assert_eq!(out.lines().count(), 2, "{out}");
        let (_, out, _) = run(&["--cwd", "/tmp/"], Ui::Plain);
        assert!(out.contains("make test"), "tabs are shown as spaces: {out}");
        let (_, out, _) = run(&["--pane", "%9"], Ui::Plain);
        assert!(out.is_empty());
        let (_, out, _) = run(&["--since", "2026-11-08T12:01", "--until", "now"], Ui::Plain);
        assert_eq!(out.lines().count(), 1);
        assert!(out.contains("bob@box"));
    }

    /// `--tag` repeats or takes a comma list; names are normalised, sorted, deduplicated.
    #[test]
    fn tags_reach_the_query() {
        let q = build_query(&args(&["--tag", "Ops,db", "--tag", "x", "--tag", "ops"]), &env()).unwrap();
        assert_eq!(q.filters.tags, ["db", "ops", "x"]);
        assert!(build_query(&args(&[]), &env()).unwrap().filters.tags.is_empty());
        let err = build_query(&args(&["--tag", "two words"]), &env()).unwrap_err();
        assert!(err.to_string().contains("--tag"), "{err}");
        // The store has no annotations: nothing carries the tag.
        let (c, out, _) = run(&["--tag", "ops"], Ui::Plain);
        assert_eq!((c, out.as_str()), (1, ""));
    }

    #[test]
    fn no_match_exits_1() {
        let (c, out, err) = run(&["does-not-exist"], Ui::Plain);
        assert_eq!((c, out.as_str()), (1, ""));
        assert!(err.contains("no matches"));
    }

    #[test]
    fn bad_arguments_are_errors() {
        let store = store();
        let e = |argv: &[&str]| {
            let mut sink = Vec::new();
            search(&args(argv), &store, &env(), Ui::Plain, &mut Vec::new(), &mut sink)
                .unwrap_err()
                .to_string()
        };
        assert!(e(&["--exit", "maybe"]).contains("--exit"));
        assert!(e(&["--since", "soon"]).contains("--since"));
        assert!(e(&["--since", "1h", "--until", "2h"]).contains("after"));
        assert!(e(&["--out"]).contains("needs a search term"));
    }

    #[test]
    fn json_lines_carry_the_entry() {
        let (c, out, _) = run(&["--json", "grep"], Ui::Json);
        assert_eq!(c, 0);
        let v: serde_json::Value = serde_json::from_str(out.lines().next().unwrap()).unwrap();
        assert_eq!(v["entry"], format!("{U1}:2"));
        assert_eq!(v["exit"], 1);
        assert_eq!(v["typed"], "grep needle notes.txt");
        assert!(v.get("url").is_none());
    }

    #[test]
    fn limit_pages_through_the_cursor() {
        let store = store();
        let reader = store.reader().unwrap();
        let page = collect(&*reader, SearchQuery::new(""), 3).unwrap();
        assert_eq!(page.hits.len(), 3);
        let all = collect(&*reader, SearchQuery::new(""), 1000).unwrap();
        assert_eq!(all.hits.len(), 4);
        assert!(all.hits.windows(2).all(|w| w[0].ts >= w[1].ts));
    }

    #[test]
    fn enter_prints_cast_and_show_command_before_the_web_server() {
        let picker = FakePicker {
            want: Some("grep needle"),
            ..FakePicker::default()
        };
        let (c, out, _) = run(&["needle"], Ui::Pick(&picker));
        assert_eq!(c, 0);
        assert_eq!(
            out,
            format!("/home/alice/logs/2026-11/box_{U1}.cast\ncinelog show {U1}:2\n")
        );
        let shown = picker.shown.borrow();
        assert_eq!(shown.len(), 1, "one page");
        assert_eq!(shown[0].len(), 2);
        assert!(shown[0].iter().all(|l| l.split('\t').count() == 5));
    }

    /// A team hit's cast lives on the team server: Enter without a known server URL points
    /// at `show --team`, not at a local path.
    #[test]
    fn enter_prints_show_team_for_team_hits() {
        let picker = FakePicker {
            want: Some("grep needle"),
            ..FakePicker::default()
        };
        let (c, out, _) = run(&["needle", "--team"], Ui::Pick(&picker));
        assert_eq!(c, 0);
        assert_eq!(out, format!("cinelog show --team {U1}:2\n"));
    }

    #[test]
    fn enter_prints_the_url_once_a_server_runs() {
        let store = store();
        let mut env = env();
        env.server = Some(ServerInfo {
            pid: 1,
            url: "https://box:11000".into(),
            port: 11000,
            fingerprint: String::new(),
            version: String::new(),
        });
        let picker = FakePicker {
            want: Some("cat needle"),
            ..FakePicker::default()
        };
        let (c, out, _) = capture(|o, e| search(&args(&["needle"]), &store, &env, Ui::Pick(&picker), o, e));
        assert_eq!(c, 0);
        assert_eq!(out, format!("https://box:11000/s/{U2}?cmd=1\n"));
        let at = EntryRef::At {
            uuid: U1.into(),
            t: 1.5,
        };
        assert_eq!(
            entry_url(env.server.as_ref().unwrap(), &at),
            format!("https://box:11000/s/{U1}?t=1.5")
        );
    }

    #[test]
    fn aborting_fzf_selects_nothing() {
        let picker = FakePicker::default();
        let (c, out, _) = run(&["needle"], Ui::Pick(&picker));
        assert_eq!((c, out.as_str()), (1, ""), "contract: 0, or 1 when nothing was chosen");
    }

    #[test]
    fn cwd_resolution() {
        let pwd = Some(Path::new("/home/alice/src"));
        let home = Path::new("/home/alice");
        let r = |a: &str| resolve_cwd(a, pwd, home).unwrap();
        assert_eq!(r("."), "/home/alice/src");
        assert_eq!(r("./x/../y/"), "/home/alice/src/y");
        assert_eq!(r(".."), "/home/alice");
        assert_eq!(r("~"), "/home/alice");
        assert_eq!(r("~/a//b"), "/home/alice/a/b");
        assert_eq!(r("/../etc/"), "/etc");
        assert_eq!(r("/"), "/");
        assert!(resolve_cwd(".", None, home).is_err());
    }

    #[test]
    fn colored_lines_highlight_hits_and_failures() {
        let store = store();
        let reader = store.reader().unwrap();
        let mut q = SearchQuery::new("needle");
        q.target = SearchTarget::Output;
        let page = collect(&*reader, q, 10).unwrap();
        let line = hit_line(&page.hits[0], &env(), Style::COLOR);
        assert!(
            line.contains(&format!("{}needle{}", text::HIT, text::RESET)),
            "{line:?}"
        );
        assert!(line.contains(&format!("{RED}[1]")), "{line:?}");
        let first = line.split('\t').next().unwrap();
        assert!(!first.contains('\x1b'), "the hidden id field stays plain");
    }
}
