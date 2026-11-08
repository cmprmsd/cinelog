//! `cinelog forget <uuid>[:<seq>] [--from S --to S]` (§7.6): records the target in
//! `meta.forgotten` (search hides it at once), then purges index.db unless a running indexer
//! holds the writer lock (it applies the list itself). Casts on disk are never rewritten.

use std::io::Write;

use cinelog_detect::{Pid, Principal, PrincipalRequest};
use cinelog_index::model::{EntryRef, ForgetTarget};

use super::{Store, code};
use crate::cli::ForgetArgs;

/// Who is recorded as `forgotten.by`: the user, or at EUID 0 the principal the recorder
/// would attribute this process to (§5.6: ancestor walk, then loginuid, else `root`).
/// `$SUDO_USER` is never consulted: a root shell can set it to anything.
pub fn forgetter(euid: u32, user: &str, pid: Pid, principal: impl FnOnce(&PrincipalRequest) -> Principal) -> String {
    if euid != 0 {
        return user.to_owned();
    }
    principal(&PrincipalRequest {
        euid,
        start_pid: pid,
        tmux_client_pid: None,
        claimed_user: None,
    })
    .user
}

pub fn target(args: &ForgetArgs) -> anyhow::Result<ForgetTarget> {
    let entry: EntryRef = args.id.parse()?;
    Ok(match (entry, args.from, args.to) {
        (EntryRef::Session { uuid }, None, None) => ForgetTarget::Session { uuid },
        (EntryRef::Session { uuid }, Some(t_from), Some(t_to)) => {
            anyhow::ensure!(
                t_from.is_finite() && t_to.is_finite() && t_from >= 0.0,
                "--from/--to must be non-negative seconds"
            );
            anyhow::ensure!(t_from <= t_to, "--from is after --to");
            ForgetTarget::Range { uuid, t_from, t_to }
        }
        (EntryRef::Command { uuid, seq }, None, None) => ForgetTarget::Command { uuid, seq },
        (EntryRef::Command { .. }, ..) => anyhow::bail!("--from/--to take a session id, not <uuid>:<seq>"),
        (EntryRef::At { .. }, ..) => {
            anyhow::bail!("forget takes <uuid> or <uuid>:<seq> (use --from/--to for a time range)")
        }
        (EntryRef::Session { .. }, ..) => anyhow::bail!("--from and --to go together"),
    })
}

fn describe(t: &ForgetTarget) -> String {
    match t {
        ForgetTarget::Session { uuid } => format!("session {uuid}"),
        ForgetTarget::Command { uuid, seq } => format!("command {uuid}:{seq}"),
        ForgetTarget::Range { uuid, t_from, t_to } => format!("output of {uuid} from t={t_from}s to t={t_to}s"),
    }
}

pub fn forget(
    args: &ForgetArgs,
    store: &dyn Store,
    by: &str,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> anyhow::Result<u8> {
    let target = target(args)?;
    // meta.db is the authority; index.db is derived, so a broken index never blocks a forget.
    let mut cast = None;
    if store.has_index() {
        match store.reader().and_then(|r| Ok(r.session(target.uuid())?)) {
            Ok(Some(s)) => cast = Some(s.cast_path),
            Ok(None) => writeln!(
                err,
                "cinelog: forget: {} is not in the index; recorded anyway, so indexing never adds it",
                target.uuid()
            )?,
            Err(e) => writeln!(
                err,
                "cinelog: forget: cannot read the index ({e:#}); recording the forget anyway"
            )?,
        }
    }
    let mut meta = store.meta()?;
    let recorded = meta.forget(&target, by)?;
    writeln!(out, "forgot {} (hidden from search now)", describe(&recorded.target))?;
    match store.writer() {
        Ok(Some(mut writer)) => {
            let o = writer.apply_forgotten(&meta.forgotten()?)?;
            writeln!(
                out,
                "removed from the index: {} sessions, {} commands, {} output chunks",
                o.sessions, o.commands, o.outputs
            )?;
        }
        Ok(None) if store.has_index() => writeln!(out, "the running indexer removes it from index.db")?,
        Ok(None) => {}
        Err(e) => writeln!(
            err,
            "cinelog: forget: index.db not purged ({e:#}); the indexer applies the forget when it next opens it"
        )?,
    }
    if let Some(cast) = cast {
        writeln!(
            out,
            "the cast is unchanged on disk (web playback masks it): {}",
            cast.display()
        )?;
    }
    Ok(code::OK)
}

#[cfg(test)]
mod tests {
    use cinelog_index::model::{SearchQuery, SearchTarget};

    use super::super::testutil::*;
    use super::*;

    fn args(id: &str, from: Option<f64>, to: Option<f64>) -> ForgetArgs {
        ForgetArgs {
            id: id.into(),
            from,
            to,
        }
    }

    fn hits(store: &MemStore, term: &str) -> usize {
        let mut q = SearchQuery::new(term);
        q.target = SearchTarget::All;
        store.reader().unwrap().search(&q).unwrap().hits.len()
    }

    #[test]
    fn targets() {
        let t = |id: &str, f, to| target(&args(id, f, to));
        assert_eq!(t(U1, None, None).unwrap(), ForgetTarget::Session { uuid: U1.into() });
        assert_eq!(
            t(&format!("{U1}:2"), None, None).unwrap(),
            ForgetTarget::Command {
                uuid: U1.into(),
                seq: 2
            }
        );
        assert_eq!(
            t(U1, Some(1.0), Some(2.0)).unwrap(),
            ForgetTarget::Range {
                uuid: U1.into(),
                t_from: 1.0,
                t_to: 2.0
            }
        );
        assert!(t(U1, Some(3.0), Some(2.0)).is_err());
        assert!(t(U1, Some(-1.0), Some(2.0)).is_err());
        assert!(t(U1, Some(f64::NAN), Some(2.0)).is_err());
        assert!(t(&format!("{U1}:2"), Some(1.0), Some(2.0)).is_err());
        assert!(t(&format!("{U1}@2"), None, None).is_err());
        assert!(t("nope", None, None).is_err());
    }

    fn principal(user: &str) -> Principal {
        Principal {
            scope: cinelog_proto::Scope::Team,
            user: user.into(),
            sudo_user: Some(user.into()),
            claimed_user: None,
            attribution: cinelog_proto::Attribution::Loginuid,
            login_via: None,
        }
    }

    /// Regression: at EUID 0 the forgetter came from `$SUDO_USER`, which a root shell can
    /// set to anyone; it is now the recorder's principal of this process.
    #[test]
    fn forgetter_is_the_principal_at_euid_0() {
        let mut asked = None;
        let by = forgetter(0, "root", 4242, |req| {
            asked = Some(req.clone());
            principal("carol")
        });
        assert_eq!(by, "carol");
        let req = asked.unwrap();
        assert_eq!((req.euid, req.start_pid, req.claimed_user), (0, 4242, None));
        assert_eq!(forgetter(0, "root", 1, |_| principal("root")), "root");
        let by = forgetter(1000, "bob", 1, |_| panic!("no attribution walk for a personal scope"));
        assert_eq!(by, "bob");
    }

    /// A [`MemStore`] whose index.db exists but cannot be opened.
    struct BrokenIndex(MemStore);
    impl Store for BrokenIndex {
        fn reader(&self) -> anyhow::Result<Box<dyn cinelog_index::Searcher>> {
            anyhow::bail!("schema version 9 is newer than this cinelog supports")
        }
        fn meta(&self) -> anyhow::Result<Box<dyn cinelog_index::MetaStore>> {
            self.0.meta()
        }
        fn writer(&self) -> anyhow::Result<Option<Box<dyn cinelog_index::IndexWriter>>> {
            anyhow::bail!("schema version 9 is newer than this cinelog supports")
        }
        fn has_index(&self) -> bool {
            true
        }
    }

    /// Regression: an unreadable index.db made forget fail before meta.db was written.
    #[test]
    fn an_unreadable_index_does_not_block_the_forget() {
        let store = BrokenIndex(store());
        let (c, out, err) = capture(|o, e| forget(&args(U1, None, None), &store, "alice", o, e));
        assert_eq!(c, 0, "{err}");
        assert_eq!(out, format!("forgot session {U1} (hidden from search now)\n"));
        assert!(err.contains("cannot read the index (schema version 9"), "{err}");
        assert!(err.contains("index.db not purged"), "{err}");
        let f = store.meta().unwrap().forgotten().unwrap();
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].target, ForgetTarget::Session { uuid: U1.into() });
    }

    #[test]
    fn command_is_recorded_and_purged() {
        let store = store();
        assert_eq!(hits(&store, "needle"), 3);
        let (c, out, err) = capture(|o, e| forget(&args(&format!("{U1}:2"), None, None), &store, "alice", o, e));
        assert_eq!(c, 0, "{err}");
        assert!(
            out.starts_with(&format!("forgot command {U1}:2 (hidden from search now)\n")),
            "{out}"
        );
        assert!(
            out.contains("removed from the index: 0 sessions, 1 commands, 1 output chunks"),
            "{out}"
        );
        assert!(out.contains("the cast is unchanged on disk"));
        assert_eq!(hits(&store, "needle"), 1, "only bob's command is left");
        let f = store.meta().unwrap().forgotten().unwrap();
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].by, "alice");
    }

    #[test]
    fn locked_writer_leaves_the_purge_to_the_indexer() {
        let mut store = store();
        store.locked = true;
        let (c, out, _) = capture(|o, e| forget(&args(U2, None, None), &store, "bob", o, e));
        assert_eq!(c, 0);
        assert!(out.contains("the running indexer removes it"), "{out}");
        assert_eq!(hits(&store, "cat needle"), 0, "hidden at query time");
        assert_eq!(store.meta().unwrap().forgotten().unwrap().len(), 1);
    }

    #[test]
    fn unknown_sessions_are_recorded_with_a_warning() {
        let store = store();
        let other = "01934f5e-7b2a-7c3d-8e4f-000000000000";
        let (c, _, err) = capture(|o, e| forget(&args(other, Some(0.0), Some(5.0)), &store, "a", o, e));
        assert_eq!(c, 0);
        assert!(err.contains("not in the index; recorded anyway"), "{err}");
        let again = capture(|o, e| forget(&args(other, Some(0.0), Some(5.0)), &store, "a", o, e));
        assert_eq!(again.0, 0);
        assert_eq!(store.meta().unwrap().forgotten().unwrap().len(), 1, "idempotent");
    }

    #[test]
    fn works_before_the_index_exists() {
        let mut store = store();
        store.has_index = false;
        let (c, out, err) = capture(|o, e| forget(&args(U1, None, None), &store, "a", o, e));
        assert_eq!(c, 0);
        assert_eq!(out, format!("forgot session {U1} (hidden from search now)\n"));
        assert!(err.is_empty());
    }
}
