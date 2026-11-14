//! `GET /api/tree` (docs/API.md §5.3): host › tmux server › tmux session › window › pane ›
//! sessions, plus non-tmux shells grouped by `holder_comm`.

use std::collections::{BTreeMap, HashMap, HashSet};

use super::db::{Forgets, IndexDb, SessionFilter};
use super::types::{
    Session, SessionState, TmuxAttach, Tree, TreeAttachedShell, TreeHost, TreeNode, TreePane, TreeParams,
    TreeShellGroup, TreeTmuxServer, TreeTmuxSession, TreeWindow,
};
use crate::error::{Result, WebError};
use crate::state::WebContext;

pub(crate) fn tree(ctx: &WebContext, p: &TreeParams, now: f64) -> Result<Tree> {
    for (name, v) in [("from", p.from), ("to", p.to)] {
        if v.is_some_and(|x| !x.is_finite()) {
            return Err(WebError::BadRequest(format!("{name} must be a finite number")));
        }
    }
    let filter = SessionFilter {
        live: p.live,
        active_from: p.from,
        active_to: p.to,
        include_hidden: p.hidden.unwrap_or(false),
        exclude: Forgets::load(ctx)?.sessions(),
        ..SessionFilter::default()
    };
    let db = IndexDb::open(ctx)?;
    let rows = db.sessions(&filter, None, None)?;
    // Legacy casts have no recorder markers.
    let ids: Vec<i64> = rows.iter().filter(|r| !r.session.legacy).map(|r| r.id).collect();
    let mut by_id = db.tmux_attaches(&ids)?;
    let mut attaches = Attaches::new();
    let mut sessions = Vec::with_capacity(rows.len());
    for r in rows {
        if let Some(list) = by_id.remove(&r.id) {
            attaches.insert(r.session.uuid.clone(), list);
        }
        sessions.push(r.session);
    }
    Ok(build(sessions, &attaches, now))
}

/// The tmux sessions a session's tmux client attached to, by session uuid.
pub(crate) type Attaches = HashMap<String, Vec<TmuxAttach>>;

/// When a session was last active: its end, or `now` while live.
fn activity(s: &Session, now: f64) -> f64 {
    match (s.state, s.ended_at) {
        (SessionState::Live, _) => now,
        (_, Some(e)) => e,
        (_, None) => s.started_at,
    }
}

/// Window and pane indexes sort numerically; anything else after them, by text.
pub(crate) fn window_key(w: Option<&str>) -> (u8, u64, String) {
    match w {
        Some(s) => match s.parse::<u64>() {
            Ok(n) => (0, n, String::new()),
            Err(_) => (1, 0, s.to_owned()),
        },
        None => (2, 0, String::new()),
    }
}

/// Nodes for `roots` (newest first), each with its nested children (oldest first).
struct Forest<'a> {
    children: HashMap<String, Vec<Session>>,
    attaches: &'a Attaches,
}

impl Forest<'_> {
    fn reach(&self, uuid: &str, reached: &mut HashSet<String>) {
        if !reached.insert(uuid.to_owned()) {
            return;
        }
        for k in self.children.get(uuid).into_iter().flatten() {
            self.reach(&k.uuid, reached);
        }
    }

    fn node(&self, s: &Session, seen: &mut HashSet<String>) -> TreeNode {
        seen.insert(s.uuid.clone());
        let mut children = Vec::new();
        for k in self.children.get(&s.uuid).into_iter().flatten() {
            if !seen.contains(&k.uuid) {
                children.push(self.node(k, seen));
            }
        }
        TreeNode {
            uuid: s.uuid.clone(),
            user: s.user.clone(),
            state: s.state,
            started_at: s.started_at,
            ended_at: s.ended_at,
            legacy: s.legacy,
            hidden: s.hidden,
            commands: s.commands,
            sudo_user: s.sudo_user.clone().filter(|u| !u.is_empty()),
            attached: self.attaches.get(&s.uuid).cloned().unwrap_or_default(),
            children,
        }
    }

    fn nodes(&self, mut roots: Vec<&Session>, seen: &mut HashSet<String>) -> Vec<TreeNode> {
        roots.sort_by(|a, b| b.started_at.total_cmp(&a.started_at).then_with(|| b.uuid.cmp(&a.uuid)));
        let mut out = Vec::with_capacity(roots.len());
        for s in roots {
            if !seen.contains(&s.uuid) {
                out.push(self.node(s, seen));
            }
        }
        out
    }
}

/// A node and everything nested in it.
fn recordings(nodes: &[TreeNode]) -> u32 {
    nodes.iter().map(|n| 1 + recordings(&n.children)).sum()
}

#[derive(Default)]
struct PaneAcc<'a> {
    sessions: Vec<&'a Session>,
    /// Index of the pane in its window, from the newest session that knows it.
    index: Option<(f64, String)>,
}

#[derive(Default)]
struct WindowAcc<'a> {
    panes: BTreeMap<Option<String>, PaneAcc<'a>>,
    /// Name of the newest session's window.
    name: Option<(f64, String)>,
}

#[derive(Default)]
struct TmuxSessionAcc<'a> {
    windows: BTreeMap<(u8, u64, String), (Option<String>, WindowAcc<'a>)>,
    name: Option<(f64, String)>,
    activity: f64,
    live: bool,
}

struct ServerAcc<'a> {
    sessions: HashMap<Option<String>, TmuxSessionAcc<'a>>,
    activity: f64,
    first_at: f64,
    live: bool,
}

impl Default for ServerAcc<'_> {
    fn default() -> Self {
        Self {
            sessions: HashMap::new(),
            activity: 0.0,
            first_at: f64::INFINITY,
            live: false,
        }
    }
}

#[derive(Default)]
struct HostAcc<'a> {
    servers: HashMap<String, ServerAcc<'a>>,
    shells: HashMap<Option<String>, Vec<&'a Session>>,
}

fn newer_name(slot: &mut Option<(f64, String)>, at: f64, name: Option<&String>) {
    if let Some(n) = name
        && slot.as_ref().is_none_or(|(t, _)| at >= *t)
    {
        *slot = Some((at, n.clone()));
    }
}

fn attached_shell(s: &Session) -> TreeAttachedShell {
    TreeAttachedShell {
        uuid: s.uuid.clone(),
        user: s.user.clone(),
        state: s.state,
        started_at: s.started_at,
        holder_comm: s.holder_comm.clone(),
        tty: s.tty.clone(),
        tmux: s.tmux.clone(),
    }
}

pub(crate) fn build(sessions: Vec<Session>, attaches: &Attaches, now: f64) -> Tree {
    // (host, tmux server, tmux session id) → the shells whose client attached to it.
    let mut clients: HashMap<(&str, &str, &str), Vec<&Session>> = HashMap::new();
    for s in &sessions {
        for a in attaches.get(&s.uuid).into_iter().flatten() {
            clients
                .entry((s.host.as_str(), a.server.as_str(), a.session_id.as_str()))
                .or_default()
                .push(s);
        }
    }
    for list in clients.values_mut() {
        list.sort_by(|a, b| b.started_at.total_cmp(&a.started_at).then_with(|| b.uuid.cmp(&a.uuid)));
    }
    let present: HashSet<&str> = sessions.iter().map(|s| s.uuid.as_str()).collect();
    let mut children: HashMap<String, Vec<Session>> = HashMap::new();
    let mut roots = Vec::new();
    for s in &sessions {
        match &s.parent_uuid {
            Some(p) if p != &s.uuid && present.contains(p.as_str()) => {
                children.entry(p.clone()).or_default().push(s.clone());
            }
            _ => roots.push(s),
        }
    }
    for kids in children.values_mut() {
        kids.sort_by(|a, b| a.started_at.total_cmp(&b.started_at).then_with(|| a.uuid.cmp(&b.uuid)));
    }
    let forest = Forest { children, attaches };
    // Sessions in a parent cycle are reachable from no root: the oldest of each cycle
    // becomes one.
    let mut reached = HashSet::new();
    for r in &roots {
        forest.reach(&r.uuid, &mut reached);
    }
    let mut by_age: Vec<&Session> = sessions.iter().collect();
    by_age.sort_by(|a, b| a.started_at.total_cmp(&b.started_at).then_with(|| a.uuid.cmp(&b.uuid)));
    for s in by_age {
        if !reached.contains(&s.uuid) {
            forest.reach(&s.uuid, &mut reached);
            roots.push(s);
        }
    }

    let mut hosts: BTreeMap<String, HostAcc<'_>> = BTreeMap::new();
    for s in roots {
        let host = hosts.entry(s.host.clone()).or_default();
        let act = activity(s, now);
        match &s.tmux.server {
            Some(server) => {
                let srv = host.servers.entry(server.clone()).or_default();
                let live = s.state == SessionState::Live;
                srv.activity = srv.activity.max(act);
                srv.first_at = srv.first_at.min(s.started_at);
                srv.live |= live;
                let ts = srv.sessions.entry(s.tmux.session_id.clone()).or_default();
                ts.activity = ts.activity.max(act);
                ts.live |= live;
                newer_name(&mut ts.name, s.started_at, s.tmux.session.as_ref());
                let key = window_key(s.tmux.window.as_deref());
                let (_, win) = ts
                    .windows
                    .entry(key)
                    .or_insert_with(|| (s.tmux.window.clone(), WindowAcc::default()));
                newer_name(&mut win.name, s.started_at, s.tmux.window_name.as_ref());
                let pane = win.panes.entry(s.tmux.pane.clone()).or_default();
                newer_name(&mut pane.index, s.started_at, s.tmux.pane_index.as_ref());
                pane.sessions.push(s);
            }
            None => host.shells.entry(s.holder_comm.clone()).or_default().push(s),
        }
    }

    // A cycle in parent links would leave sessions unreachable from any root; `seen`
    // keeps every session listed once.
    let mut seen = HashSet::new();
    let hosts = hosts
        .into_iter()
        .map(|(host, acc)| {
            let mut servers: Vec<(String, ServerAcc<'_>)> = acc.servers.into_iter().collect();
            servers.sort_by(|a, b| b.1.activity.total_cmp(&a.1.activity).then_with(|| a.0.cmp(&b.0)));
            let tmux_servers = servers
                .into_iter()
                .map(|(server, srv)| {
                    let mut tsessions: Vec<(Option<String>, TmuxSessionAcc<'_>)> = srv.sessions.into_iter().collect();
                    tsessions.sort_by(|a, b| b.1.activity.total_cmp(&a.1.activity).then_with(|| a.0.cmp(&b.0)));
                    let sessions = tsessions
                        .into_iter()
                        .map(|(session_id, ts)| {
                            let windows: Vec<TreeWindow> = ts
                                .windows
                                .into_values()
                                .map(|(window, w)| TreeWindow {
                                    window,
                                    name: w.name.map(|(_, n)| n),
                                    panes: w
                                        .panes
                                        .into_iter()
                                        .map(|(pane, p)| TreePane {
                                            pane,
                                            index: p.index.map(|(_, i)| i),
                                            sessions: forest.nodes(p.sessions, &mut seen),
                                        })
                                        .collect(),
                                })
                                .collect();
                            let attached_from = session_id
                                .as_deref()
                                .and_then(|id| clients.get(&(host.as_str(), server.as_str(), id)))
                                .map(|list| list.iter().map(|s| attached_shell(s)).collect())
                                .unwrap_or_default();
                            TreeTmuxSession {
                                session_id,
                                name: ts.name.map(|(_, n)| n),
                                live: ts.live,
                                recordings: windows
                                    .iter()
                                    .flat_map(|w| &w.panes)
                                    .map(|p| recordings(&p.sessions))
                                    .sum(),
                                attached_from,
                                windows,
                            }
                        })
                        .collect();
                    TreeTmuxServer {
                        server,
                        first_at: srv.first_at,
                        live: srv.live,
                        sessions,
                    }
                })
                .collect();
            let mut shells: Vec<(Option<String>, Vec<&Session>)> = acc.shells.into_iter().collect();
            // Named holders by name, the unknown (`null`) group last.
            shells.sort_by(|a, b| (a.0.is_none(), &a.0).cmp(&(b.0.is_none(), &b.0)));
            TreeHost {
                host,
                tmux_servers,
                shells: shells
                    .into_iter()
                    .map(|(holder_comm, list)| TreeShellGroup {
                        holder_comm,
                        sessions: forest.nodes(list, &mut seen),
                    })
                    .collect(),
            }
        })
        .collect();
    Tree { hosts }
}

#[cfg(test)]
mod tests {
    use super::super::types::{Scope, TmuxLocation};
    use super::*;

    fn s(uuid: &str, started: f64) -> Session {
        Session {
            uuid: uuid.into(),
            host: "h".into(),
            scope: Scope::Personal,
            user: "u".into(),
            euid: None,
            sudo_user: None,
            claimed_user: None,
            attribution: None,
            login_via: None,
            tty: None,
            holder_comm: None,
            tmux: TmuxLocation::default(),
            parent_uuid: None,
            parts: 1,
            compressed: false,
            cast_bytes: None,
            started_at: started,
            ended_at: Some(started + 1.0),
            duration: None,
            exit_status: None,
            tz: None,
            cols: 80,
            rows: 24,
            term: None,
            legacy: false,
            hidden: false,
            state: SessionState::Ended,
            commands: 0,
        }
    }

    fn tmux(mut x: Session, server: &str, sid: &str, window: &str, pane: &str) -> Session {
        x.tmux = TmuxLocation {
            server: Some(server.into()),
            session_id: Some(sid.into()),
            session: Some(format!("name{sid}")),
            window: Some(window.into()),
            window_name: Some(format!("w{window}")),
            pane: Some(pane.into()),
            pane_index: None,
        };
        x
    }

    #[test]
    fn groups_and_orders() {
        let mut nested = s("n", 15.0);
        nested.parent_uuid = Some("p1".into());
        nested.holder_comm = Some("sudo".into());
        let mut live = tmux(s("p2", 5.0), "1:1", "$1", "2", "%2");
        live.state = SessionState::Live;
        live.ended_at = None;
        let mut term = s("t1", 3.0);
        term.holder_comm = Some("terminator".into());
        let mut orphan = s("o", 4.0);
        orphan.parent_uuid = Some("missing".into());
        let sessions = vec![
            tmux(s("p1", 10.0), "1:1", "$1", "10", "%1"),
            live,
            tmux(s("old", 1.0), "9:9", "$0", "0", "%0"),
            nested,
            term,
            orphan,
        ];
        let tree = build(sessions, &Attaches::new(), 100.0);
        assert_eq!(tree.hosts.len(), 1);
        let h = &tree.hosts[0];
        assert_eq!(h.tmux_servers[0].server, "1:1", "live server is the most recent");
        let windows = &h.tmux_servers[0].sessions[0].windows;
        assert_eq!(
            windows.iter().map(|w| w.window.as_deref().unwrap()).collect::<Vec<_>>(),
            ["2", "10"],
            "numeric window order"
        );
        let p1 = &windows[1].panes[0].sessions[0];
        assert_eq!(p1.uuid, "p1");
        assert_eq!(p1.children.iter().map(|c| c.uuid.as_str()).collect::<Vec<_>>(), ["n"]);
        assert_eq!(h.tmux_servers[0].sessions[0].name.as_deref(), Some("name$1"));
        // The nested session is not repeated in its holder group; an orphan stays a root.
        assert_eq!(
            h.shells.iter().map(|g| g.holder_comm.as_deref()).collect::<Vec<_>>(),
            [Some("terminator"), None]
        );
        assert_eq!(h.shells[1].sessions[0].uuid, "o");
    }

    #[test]
    fn parent_cycles_list_every_session_once() {
        let mut a = s("a", 1.0);
        a.parent_uuid = Some("b".into());
        let mut b = s("b", 2.0);
        b.parent_uuid = Some("a".into());
        let tree = build(vec![a, b], &Attaches::new(), 10.0);
        let g = &tree.hosts[0].shells[0];
        assert_eq!(g.sessions.len(), 1, "the oldest of the cycle is the root");
        assert_eq!(g.sessions[0].uuid, "a");
        assert_eq!(g.sessions[0].children[0].uuid, "b");
        assert!(g.sessions[0].children[0].children.is_empty());
    }
}
