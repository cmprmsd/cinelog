//! Annotations and tags (work package M4-C; plan §6.3, §8.4; docs/API.md §9.4).
//!
//! | Route | Body / query | Response |
//! |---|---|---|
//! | `GET /annotations` | `AnnotationParams` | `AnnotationPage` |
//! | `POST /annotations` | `AnnotationCreate` | 201 `Annotation` |
//! | `PATCH /annotations/{uuid}` | `AnnotationPatch` | `Annotation` |
//! | `DELETE /annotations/{uuid}` | | 204 |
//! | `GET /tags` | | `TagList` |
//! | `POST /tags` | `TagRequest` | `Tag` |
//!
//! Annotations live in meta.db (authoritative, keyed by session uuid and command seq), so
//! they survive `index --rebuild` and travel with the meta backup. The author is the
//! [`Principal`]; only the author changes the body or the tag set or deletes, while any
//! principal may add tags. Annotations of forgotten sessions and commands are treated as
//! absent. Every change is published on the bus after its commit.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, patch};
use cinelog_index::MetaStore;
use cinelog_index::model::{self as im, AnnotationQuery};

use super::db::{Forgets, IndexDb, blocking, parse_uuid};
use super::extract::{ApiJson, ApiPath, ApiQuery};
use super::search::limit;
use super::sessions::visible_session;
use super::types::{
    Annotation, AnnotationCreate, AnnotationEvent, AnnotationPage, AnnotationParams, AnnotationPatch,
    AnnotationRequestError, BusEvent, ChangeOp, Tag, TagList, TagRequest, normalize_tag,
};
use crate::bus::Bus;
use crate::error::{Result, WebError};
use crate::state::{AppState, Principal, WebContext};

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/annotations", get(list).post(create))
        .route("/annotations/{uuid}", patch(update).delete(delete))
        .route("/tags", get(tags).post(put_tag))
}

const DEFAULT_LIMIT: u32 = 200;
const MAX_LIMIT: u32 = 500;

/// Serialises this server's read-modify-write cycles on annotations (an `add_tags` racing
/// an edit must not lose either change) and their publication, so the bus carries changes
/// in commit order. Last write still wins between servers.
static WRITES: Mutex<()> = Mutex::new(());

/// What a change produced: the stored annotation and the tags it created.
struct Change {
    annotation: Annotation,
    new_tags: Vec<Tag>,
}

impl Change {
    /// Tag creations first, so clients know a tag before an annotation carries it.
    fn publish(self, bus: &Bus, op: ChangeOp) -> Annotation {
        for tag in self.new_tags {
            bus.publish(BusEvent::Tag(tag));
        }
        bus.publish(BusEvent::Annotation(AnnotationEvent {
            op,
            annotation: self.annotation.clone(),
        }));
        self.annotation
    }
}

async fn list(State(st): State<AppState>, ApiQuery(p): ApiQuery<AnnotationParams>) -> Result<Json<AnnotationPage>> {
    blocking(move || list_page(&st.ctx, &p)).await.map(Json)
}

async fn create(
    State(st): State<AppState>,
    Principal(by): Principal,
    ApiJson(req): ApiJson<AnnotationCreate>,
) -> Result<(StatusCode, Json<Annotation>)> {
    let a = blocking(move || create_annotation(&st.ctx, &st.bus, &req, &by, now_us)).await?;
    Ok((StatusCode::CREATED, Json(a)))
}

async fn update(
    State(st): State<AppState>,
    Principal(by): Principal,
    ApiPath(uuid): ApiPath<String>,
    ApiJson(req): ApiJson<AnnotationPatch>,
) -> Result<Json<Annotation>> {
    blocking(move || update_annotation(&st.ctx, &st.bus, &uuid, &req, &by, now_us))
        .await
        .map(Json)
}

async fn delete(
    State(st): State<AppState>,
    Principal(by): Principal,
    ApiPath(uuid): ApiPath<String>,
) -> Result<StatusCode> {
    blocking(move || delete_annotation(&st.ctx, &st.bus, &uuid, &by, now_us)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn tags(State(st): State<AppState>) -> Result<Json<TagList>> {
    blocking(move || tag_list(&st.ctx)).await.map(Json)
}

async fn put_tag(
    State(st): State<AppState>,
    Principal(by): Principal,
    ApiJson(req): ApiJson<TagRequest>,
) -> Result<Json<Tag>> {
    let (name, color) = req.validate().map_err(bad)?;
    blocking(move || {
        let _w = write_lock();
        let mut meta = cinelog_index::db::open_meta(&st.ctx.db)?;
        let tag: Tag = meta.put_tag(&name, color.as_deref())?.into();
        tracing::info!("{by} set tag {name:?} to {}", color.as_deref().unwrap_or("automatic"));
        st.bus.publish(BusEvent::Tag(tag.clone()));
        Ok(tag)
    })
    .await
    .map(Json)
}

// ------------------------------------------------------------------ the work behind the handlers

fn bad(e: AnnotationRequestError) -> WebError {
    WebError::BadRequest(e.to_string())
}

/// Epoch seconds at µs precision (the API's time resolution).
fn now_us() -> f64 {
    (crate::now() * 1e6).round() / 1e6
}

/// The `updated_at` of a write over a row last updated at `prev`: the clock, but always
/// later than `prev`, since clients keep the version with the higher `updated_at`.
fn next_update(prev: f64, now: f64) -> f64 {
    if now > prev {
        now
    } else {
        ((prev * 1e6).round() + 1.0) / 1e6
    }
}

fn write_lock() -> std::sync::MutexGuard<'static, ()> {
    // The guarded data is `()`: a panic while holding it leaves nothing inconsistent.
    WRITES.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn parse_annotation_uuid(s: &str) -> Result<String> {
    uuid::Uuid::try_parse(s.trim())
        .map(|u| u.hyphenated().to_string())
        .map_err(|_| WebError::BadRequest("not an annotation uuid".into()))
}

/// Decides whether an annotation is gone with what it annotates, by the index's rule
/// ([`Forgets::command`]): a forgotten session, a command forgotten by seq, or a command
/// inside a forgotten range.
///
/// A range forget leaves no trace of the command once the index has purged it, so in a
/// session with forgotten ranges a command that index.db no longer has counts as forgotten
/// (annotations are only made on indexed commands). Without an index, or before the session
/// is indexed, only the meta.db rules apply.
struct Visibility {
    forgets: Forgets,
    index: Option<IndexDb>,
    /// index.db row id of each session looked up so far (`None`: not indexed).
    sessions: HashMap<String, Option<i64>>,
}

impl Visibility {
    fn load(ctx: &WebContext) -> Result<Self> {
        Ok(Self {
            forgets: Forgets::load(ctx)?,
            // An index that is missing, locked or being rebuilt hides nothing extra.
            index: IndexDb::open(ctx).ok(),
            sessions: HashMap::new(),
        })
    }

    fn hidden(&mut self, a: &im::Annotation) -> Result<bool> {
        let uuid = a.session_uuid.as_str();
        if self.forgets.session(uuid) {
            return Ok(true);
        }
        let Some(seq) = a.cmd_seq else {
            return Ok(false);
        };
        if self.forgets.command_seqs(uuid).any(|s| s == seq) {
            return Ok(true);
        }
        if self.forgets.ranges(uuid).next().is_none() {
            return Ok(false);
        }
        let Some(db) = &self.index else {
            return Ok(false);
        };
        let id = match self.sessions.get(uuid) {
            Some(id) => *id,
            None => {
                let id = db.session(uuid)?.map(|s| s.id);
                self.sessions.insert(uuid.to_owned(), id);
                id
            }
        };
        let Some(id) = id else {
            return Ok(false);
        };
        Ok(match db.command(id, seq)? {
            Some(c) => self.forgets.command(uuid, &c),
            None => true,
        })
    }
}

/// `(created_at, uuid)` of the last annotation of a page.
#[derive(Debug, Clone, PartialEq)]
struct Cursor {
    created_at: f64,
    uuid: String,
}

impl Cursor {
    fn encode(&self) -> String {
        format!("{}~{}", self.created_at, self.uuid)
    }

    fn decode(s: &str) -> Result<Self> {
        let bad = || WebError::BadRequest("invalid cursor".into());
        let (ts, uuid) = s.split_once('~').ok_or_else(bad)?;
        Ok(Self {
            created_at: ts.parse::<f64>().ok().filter(|v| v.is_finite()).ok_or_else(bad)?,
            uuid: uuid::Uuid::try_parse(uuid).map_err(|_| bad())?.hyphenated().to_string(),
        })
    }
}

fn query_of(p: &AnnotationParams) -> Result<AnnotationQuery> {
    let session = p.session.as_deref().map(parse_uuid).transpose()?;
    if p.cmd_seq.is_some() && session.is_none() {
        return Err(WebError::BadRequest("cmd_seq needs session".into()));
    }
    Ok(AnnotationQuery {
        sessions: session.into_iter().collect(),
        cmd_seq: p.cmd_seq,
        tag: p.tag.as_deref().map(normalize_tag).transpose().map_err(bad)?,
        author: p
            .author
            .as_deref()
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .map(str::to_owned),
        include_deleted: p.deleted.unwrap_or(false),
        after: p
            .cursor
            .as_deref()
            .map(Cursor::decode)
            .transpose()?
            .map(|c| (c.created_at, c.uuid)),
        limit: limit(p.limit, DEFAULT_LIMIT, MAX_LIMIT)?,
    })
}

fn list_page(ctx: &WebContext, p: &AnnotationParams) -> Result<AnnotationPage> {
    let q = query_of(p)?;
    let mut vis = Visibility::load(ctx)?;
    let meta = cinelog_index::db::open_meta(&ctx.db)?;
    let rows = meta.find_annotations(&q)?;
    // The cursor follows the raw rows: hidden ones still advance it.
    let next = (rows.len() == q.limit).then(|| rows.last()).flatten().map(|a| {
        Cursor {
            created_at: a.created_at,
            uuid: a.uuid.clone(),
        }
        .encode()
    });
    let mut annotations = Vec::with_capacity(rows.len());
    for a in &rows {
        if !vis.hidden(a)? {
            annotations.push(Annotation::from(a));
        }
    }
    Ok(AnnotationPage { annotations, next })
}

/// Every tag with the number of annotations that carry it, leaving out the ones that are
/// gone with a forgotten session or command (as the list does).
fn tag_list(ctx: &WebContext) -> Result<TagList> {
    let meta = cinelog_index::db::open_meta(&ctx.db)?;
    let mut tags = meta.tags()?;
    let mut vis = Visibility::load(ctx)?;
    let sessions: Vec<String> = vis.forgets.touched().map(str::to_owned).collect();
    if !sessions.is_empty() {
        let query = AnnotationQuery {
            sessions,
            ..AnnotationQuery::default()
        };
        let mut gone: HashMap<String, u64> = HashMap::new();
        for a in meta.find_annotations(&query)? {
            if vis.hidden(&a)? {
                for t in a.tags {
                    *gone.entry(t).or_default() += 1;
                }
            }
        }
        for t in &mut tags {
            t.count = t.count.saturating_sub(gone.get(&t.name).copied().unwrap_or(0));
        }
    }
    Ok(TagList {
        tags: tags.into_iter().map(Tag::from).collect(),
    })
}

/// Tag names as a set.
fn tag_names(meta: &dyn MetaStore) -> Result<HashSet<String>> {
    Ok(meta.tags()?.into_iter().map(|t| t.name).collect())
}

/// Writes `row` and reads back what is stored, with the tags that did not exist `before`.
fn store(meta: &mut dyn MetaStore, row: &im::Annotation, before: &HashSet<String>) -> Result<Change> {
    meta.put_annotation(row)?;
    let stored = meta
        .annotation(&row.uuid)?
        .ok_or_else(|| WebError::Internal("annotation vanished after its write".into()))?;
    let new_tags = if stored.tags.iter().all(|t| before.contains(t)) {
        Vec::new()
    } else {
        meta.tags()?
            .into_iter()
            .filter(|t| !before.contains(&t.name) && stored.tags.contains(&t.name))
            .map(Tag::from)
            .collect()
    };
    Ok(Change {
        annotation: Annotation::from(&stored),
        new_tags,
    })
}

// The writers below read the clock and publish while holding the write lock, so that
// `updated_at` and the bus order both follow the commit order.

fn create_annotation(
    ctx: &WebContext,
    bus: &Bus,
    req: &AnnotationCreate,
    by: &str,
    clock: impl Fn() -> f64,
) -> Result<Annotation> {
    let new = req.validate().map_err(bad)?;
    let forgets = Forgets::load(ctx)?;
    let db = IndexDb::open(ctx)?;
    let session = visible_session(&db, &forgets, &new.session_uuid)?;
    if let Some(seq) = new.cmd_seq {
        let c = db.command(session.id, seq)?.ok_or(WebError::NotFound)?;
        if forgets.command(&new.session_uuid, &c) {
            return Err(WebError::NotFound);
        }
    }
    drop(db);
    let _w = write_lock();
    let row = new.into_row(uuid::Uuid::now_v7().hyphenated().to_string(), by, clock());
    let mut meta = cinelog_index::db::open_meta(&ctx.db)?;
    let before = tag_names(meta.as_ref())?;
    let change = store(meta.as_mut(), &row, &before)?;
    tracing::info!("{by} annotated session {}", row.session_uuid);
    Ok(change.publish(bus, ChangeOp::Created))
}

/// The live (not deleted, not hidden) annotation `uuid` of `meta`.
fn existing(ctx: &WebContext, meta: &dyn MetaStore, uuid: &str) -> Result<im::Annotation> {
    let row = meta
        .annotation(uuid)?
        .filter(|a| a.deleted_at.is_none())
        .ok_or(WebError::NotFound)?;
    if Visibility::load(ctx)?.hidden(&row)? {
        return Err(WebError::NotFound);
    }
    Ok(row)
}

fn forbidden(what: &str) -> WebError {
    WebError::Forbidden(format!("only the author can {what} this annotation"))
}

fn update_annotation(
    ctx: &WebContext,
    bus: &Bus,
    uuid: &str,
    req: &AnnotationPatch,
    by: &str,
    clock: impl Fn() -> f64,
) -> Result<Annotation> {
    let uuid = parse_annotation_uuid(uuid)?;
    let change = req.validate().map_err(bad)?;
    let _w = write_lock();
    let mut meta = cinelog_index::db::open_meta(&ctx.db)?;
    let mut row = existing(ctx, meta.as_ref(), &uuid)?;
    if change.needs_author() && row.author != by {
        return Err(forbidden("edit"));
    }
    let before = tag_names(meta.as_ref())?;
    let now = next_update(row.updated_at, clock());
    change.apply(&mut row, now).map_err(bad)?;
    Ok(store(meta.as_mut(), &row, &before)?.publish(bus, ChangeOp::Updated))
}

fn delete_annotation(ctx: &WebContext, bus: &Bus, uuid: &str, by: &str, clock: impl Fn() -> f64) -> Result<Annotation> {
    let uuid = parse_annotation_uuid(uuid)?;
    let _w = write_lock();
    let mut meta = cinelog_index::db::open_meta(&ctx.db)?;
    let mut row = existing(ctx, meta.as_ref(), &uuid)?;
    if row.author != by {
        return Err(forbidden("delete"));
    }
    let now = next_update(row.updated_at, clock());
    row.deleted_at = Some(now);
    row.updated_at = now;
    let before = tag_names(meta.as_ref())?;
    Ok(store(meta.as_mut(), &row, &before)?.publish(bus, ChangeOp::Deleted))
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use tokio::sync::broadcast::Receiver;
    use tower::ServiceExt;

    use super::*;
    use crate::api::testkit::{Cast, Kit, Meta, uuid};

    /// Session 1: three commands; session 2: one command. Returns the cast paths.
    fn fixture(kit: &Kit) -> [std::path::PathBuf; 2] {
        let dir = kit.dir.path();
        let mut a = Cast::new(dir, 1, 80, 24, &Meta::default());
        let t = a.command(1.0, 1, "ls", &["a b"], 0);
        let t = a.command(t + 1.0, 2, "make deploy", &["done"], 0);
        let t = a.command(t + 1.0, 3, "false", &[], 1);
        a.exit(t + 0.5);
        let mut b = Cast::new(dir, 2, 80, 24, &Meta::default());
        let t = b.command(0.5, 1, "id", &["uid=1000"], 0);
        b.exit(t + 0.5);
        kit.index(&a.path, None);
        kit.index(&b.path, None);
        [a.path, b.path]
    }

    fn router(kit: &Kit) -> Router {
        // The principal comes from a test header (default alice), so tests can act as others.
        Router::new()
            .merge(routes())
            .layer(axum::middleware::from_fn(
                |mut req: Request<Body>, next: axum::middleware::Next| async move {
                    let who = req
                        .headers()
                        .get("x-test-principal")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("alice")
                        .to_owned();
                    req.extensions_mut().insert(Principal(who));
                    next.run(req).await
                },
            ))
            .with_state(kit.state.clone())
    }

    async fn call(kit: &Kit, method: &str, uri: &str, who: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header("x-test-principal", who);
        let body = match body {
            Some(b) => {
                req = req.header(header::CONTENT_TYPE, "application/json");
                Body::from(serde_json::to_vec(&b).unwrap())
            }
            None => Body::empty(),
        };
        let resp = router(kit).oneshot(req.body(body).unwrap()).await.unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    async fn get(kit: &Kit, uri: &str) -> Value {
        let (status, v) = call(kit, "GET", uri, "alice", None).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {v}");
        v
    }

    async fn post(kit: &Kit, who: &str, body: Value) -> (StatusCode, Value) {
        call(kit, "POST", "/annotations", who, Some(body)).await
    }

    async fn patch(kit: &Kit, who: &str, uuid: &str, body: Value) -> (StatusCode, Value) {
        call(kit, "PATCH", &format!("/annotations/{uuid}"), who, Some(body)).await
    }

    fn drain(rx: &mut Receiver<BusEvent>) -> Vec<BusEvent> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    fn uuids(page: &Value) -> Vec<String> {
        page["annotations"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["uuid"].as_str().unwrap().to_owned())
            .collect()
    }

    #[tokio::test]
    async fn create_validates_targets_and_publishes() {
        let kit = Kit::new(8);
        fixture(&kit);
        let u = uuid(1);
        for (body, status) in [
            (
                json!({"session_uuid": "nope", "cmd_seq": 1, "body": "x"}),
                StatusCode::BAD_REQUEST,
            ),
            (json!({"session_uuid": u, "body": "x"}), StatusCode::BAD_REQUEST),
            (json!({"session_uuid": u, "cmd_seq": 1}), StatusCode::BAD_REQUEST),
            (
                json!({"session_uuid": u, "cmd_seq": 1, "t_from": 1.0, "t_to": 2.0, "body": "x"}),
                StatusCode::BAD_REQUEST,
            ),
            (
                json!({"session_uuid": u, "t_from": 3.0, "t_to": 2.0, "body": "x"}),
                StatusCode::BAD_REQUEST,
            ),
            (
                json!({"session_uuid": u, "cmd_seq": 1, "tags": ["bad tag"]}),
                StatusCode::BAD_REQUEST,
            ),
            (
                json!({"session_uuid": u, "cmd_seq": 1, "body": "x".repeat(16 * 1024 + 1)}),
                StatusCode::BAD_REQUEST,
            ),
            (
                json!({"session_uuid": uuid(9), "cmd_seq": 1, "body": "x"}),
                StatusCode::NOT_FOUND,
            ),
            (
                json!({"session_uuid": u, "cmd_seq": 42, "body": "x"}),
                StatusCode::NOT_FOUND,
            ),
        ] {
            assert_eq!(post(&kit, "alice", body.clone()).await.0, status, "{body}");
        }

        let mut bus = kit.state.bus.subscribe();
        let (status, a) = post(
            &kit,
            "alice",
            json!({"session_uuid": u.to_uppercase(), "cmd_seq": 2, "body": "deploys prod  \n", "tags": ["Prod", "incident-7"]}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{a}");
        assert_eq!(a["session"], u);
        assert_eq!(a["cmd_seq"], 2);
        assert_eq!(a["author"], "alice");
        assert_eq!(a["body"], "deploys prod");
        assert_eq!(a["tags"], json!(["incident-7", "prod"]));
        assert_eq!(a["created_at"], a["updated_at"]);
        assert!(a["deleted_at"].is_null() && a["t_from"].is_null());
        let id = uuid::Uuid::parse_str(a["uuid"].as_str().unwrap()).unwrap();
        assert_eq!(id.get_version_num(), 7);

        let events = drain(&mut bus);
        let names: Vec<_> = events.iter().map(BusEvent::name).collect();
        assert_eq!(names, ["tag", "tag", "annotation"], "new tags first");
        match &events[2] {
            BusEvent::Annotation(e) => {
                assert_eq!(e.op, ChangeOp::Created);
                assert_eq!(serde_json::to_value(&e.annotation).unwrap(), a);
            }
            other => panic!("{other:?}"),
        }

        // A span; an existing tag publishes no tag event.
        let (status, s) = post(
            &kit,
            "bob",
            json!({"session_uuid": u, "t_from": 0.5, "t_to": 2.25, "tags": ["prod"]}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!((s["t_from"].as_f64(), s["t_to"].as_f64()), (Some(0.5), Some(2.25)));
        assert_eq!(s["body"], "");
        assert!(s["cmd_seq"].is_null());
        assert_eq!(
            drain(&mut bus).iter().map(BusEvent::name).collect::<Vec<_>>(),
            ["annotation"]
        );
    }

    #[tokio::test]
    async fn listing_filters_and_pages() {
        let kit = Kit::new(8);
        fixture(&kit);
        let (u1, u2) = (uuid(1), uuid(2));
        let mut made = Vec::new();
        for (who, body) in [
            (
                "alice",
                json!({"session_uuid": u1, "cmd_seq": 1, "body": "one", "tags": ["a"]}),
            ),
            (
                "bob",
                json!({"session_uuid": u1, "cmd_seq": 2, "body": "two", "tags": ["a", "b"]}),
            ),
            (
                "alice",
                json!({"session_uuid": u1, "t_from": 0.0, "t_to": 1.0, "body": "three"}),
            ),
            (
                "alice",
                json!({"session_uuid": u2, "cmd_seq": 1, "body": "four", "tags": ["b"]}),
            ),
        ] {
            let (status, a) = post(&kit, who, body).await;
            assert_eq!(status, StatusCode::CREATED);
            made.push(a["uuid"].as_str().unwrap().to_owned());
        }
        assert_eq!(uuids(&get(&kit, "/annotations").await), made, "oldest first");
        assert_eq!(
            uuids(&get(&kit, &format!("/annotations?session={u1}")).await),
            made[..3]
        );
        assert_eq!(
            uuids(&get(&kit, &format!("/annotations?session={u1}&cmd_seq=2")).await),
            [made[1].clone()]
        );
        assert_eq!(
            uuids(&get(&kit, "/annotations?tag=B").await),
            [made[1].clone(), made[3].clone()]
        );
        assert_eq!(uuids(&get(&kit, "/annotations?author=bob").await), [made[1].clone()]);

        let mut seen = Vec::new();
        let mut uri = "/annotations?limit=3".to_owned();
        loop {
            let page = get(&kit, &uri).await;
            seen.extend(uuids(&page));
            match page["next"].as_str() {
                Some(next) => uri = format!("/annotations?limit=3&cursor={}", urlencode(next)),
                None => break,
            }
        }
        assert_eq!(seen, made);
        let two = get(&kit, "/annotations?limit=2").await;
        assert!(two["next"].is_string());
        let rest = get(
            &kit,
            &format!(
                "/annotations?limit=2&cursor={}",
                urlencode(two["next"].as_str().unwrap())
            ),
        )
        .await;
        assert_eq!(uuids(&rest), made[2..]);

        for bad in [
            "/annotations?limit=0",
            "/annotations?limit=501",
            "/annotations?cursor=garbage",
            "/annotations?cursor=1.5~nope",
            "/annotations?tag=no%20spaces",
            "/annotations?cmd_seq=1",
            "/annotations?session=x",
            "/annotations?deleted=maybe",
        ] {
            assert_eq!(
                call(&kit, "GET", bad, "alice", None).await.0,
                StatusCode::BAD_REQUEST,
                "{bad}"
            );
        }

        let tags = get(&kit, "/tags").await;
        assert_eq!(
            tags,
            json!({"tags": [{"name": "a", "color": null, "count": 2}, {"name": "b", "color": null, "count": 2}]})
        );
    }

    fn urlencode(s: &str) -> String {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' => (b as char).to_string(),
                _ => format!("%{b:02X}"),
            })
            .collect()
    }

    #[tokio::test]
    async fn only_the_author_edits_and_deletes_anyone_adds_tags() {
        let kit = Kit::new(8);
        fixture(&kit);
        let (_, a) = post(
            &kit,
            "alice",
            json!({"session_uuid": uuid(1), "cmd_seq": 1, "body": "mine", "tags": ["x"]}),
        )
        .await;
        let id = a["uuid"].as_str().unwrap().to_owned();
        let mut bus = kit.state.bus.subscribe();

        for body in [
            json!({"body": "theirs"}),
            json!({"tags": []}),
            json!({"body": "b", "add_tags": ["y"]}),
        ] {
            let (status, err) = patch(&kit, "bob", &id, body.clone()).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
            assert_eq!(err["error"], "forbidden");
        }
        assert_eq!(
            call(&kit, "DELETE", &format!("/annotations/{id}"), "bob", None).await.0,
            StatusCode::FORBIDDEN
        );
        assert!(drain(&mut bus).is_empty(), "refused changes publish nothing");

        let (status, b) = patch(&kit, "bob", &id, json!({"add_tags": ["Reviewed", "x"]})).await;
        assert_eq!(status, StatusCode::OK, "{b}");
        assert_eq!(b["tags"], json!(["reviewed", "x"]));
        assert_eq!(b["body"], "mine");
        assert_eq!(b["author"], "alice", "adding tags does not take over the annotation");
        let names: Vec<_> = drain(&mut bus).iter().map(BusEvent::name).collect();
        assert_eq!(names, ["tag", "annotation"]);

        for (body, status) in [
            (json!({}), StatusCode::BAD_REQUEST),
            (json!({"add_tags": []}), StatusCode::BAD_REQUEST),
            (json!({"body": "", "tags": []}), StatusCode::BAD_REQUEST),
            (json!({"tags": ["no way"]}), StatusCode::BAD_REQUEST),
        ] {
            assert_eq!(patch(&kit, "alice", &id, body.clone()).await.0, status, "{body}");
        }
        assert_eq!(
            patch(&kit, "alice", "zzz", json!({"body": "x"})).await.0,
            StatusCode::BAD_REQUEST
        );
        let unknown = uuid::Uuid::now_v7().to_string();
        assert_eq!(
            patch(&kit, "alice", &unknown, json!({"body": "x"})).await.0,
            StatusCode::NOT_FOUND
        );

        let (status, c) = patch(&kit, "alice", &id, json!({"body": "edited", "tags": ["final"]})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            (c["body"].as_str(), c["tags"].clone()),
            (Some("edited"), json!(["final"]))
        );
        assert!(c["updated_at"].as_f64() >= a["updated_at"].as_f64());
        assert_eq!(c["created_at"], a["created_at"]);
        drain(&mut bus);

        let (status, _) = call(&kit, "DELETE", &format!("/annotations/{id}"), "alice", None).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        match drain(&mut bus).as_slice() {
            [BusEvent::Annotation(e)] => {
                assert_eq!(e.op, ChangeOp::Deleted);
                assert!(e.annotation.deleted_at.is_some());
                assert_eq!(Some(e.annotation.updated_at), e.annotation.deleted_at);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            call(&kit, "DELETE", &format!("/annotations/{id}"), "alice", None)
                .await
                .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            patch(&kit, "alice", &id, json!({"body": "x"})).await.0,
            StatusCode::NOT_FOUND
        );
        assert!(uuids(&get(&kit, "/annotations").await).is_empty());
        let all = get(&kit, "/annotations?deleted=true").await;
        assert_eq!(uuids(&all), [id]);
        assert_eq!(all["annotations"][0]["body"], "edited", "soft delete keeps the row");
        let tags = get(&kit, "/tags").await;
        assert!(tags["tags"].as_array().unwrap().iter().all(|t| t["count"] == 0));
    }

    #[tokio::test]
    async fn tags_are_normalised_and_published() {
        let kit = Kit::new(8);
        let mut bus = kit.state.bus.subscribe();
        let (status, t) = call(
            &kit,
            "POST",
            "/tags",
            "bob",
            Some(json!({"name": " Ops/DB ", "color": "#F80"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(t, json!({"name": "ops/db", "color": "#ff8800", "count": 0}));
        let (_, t2) = call(&kit, "POST", "/tags", "alice", Some(json!({"name": "ops/db"}))).await;
        assert_eq!(t2["color"], Value::Null, "absent colour: automatic");
        assert_eq!(drain(&mut bus).len(), 2);
        for body in [
            json!({"name": "a b"}),
            json!({"name": "x", "color": "red"}),
            json!({"color": "#fff"}),
        ] {
            assert_eq!(
                call(&kit, "POST", "/tags", "alice", Some(body.clone())).await.0,
                StatusCode::BAD_REQUEST,
                "{body}"
            );
        }
        assert_eq!(get(&kit, "/tags").await["tags"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn forgotten_targets_are_gone() {
        let kit = Kit::new(8);
        fixture(&kit);
        let (u1, u2) = (uuid(1), uuid(2));
        let (_, on_cmd) = post(&kit, "alice", json!({"session_uuid": u1, "cmd_seq": 2, "body": "c2"})).await;
        let (_, other) = post(&kit, "alice", json!({"session_uuid": u1, "cmd_seq": 3, "body": "c3"})).await;
        let (_, on_s2) = post(
            &kit,
            "alice",
            json!({"session_uuid": u2, "t_from": 0.0, "t_to": 1.0, "body": "s2"}),
        )
        .await;
        kit.cli_forget(im::ForgetTarget::Command {
            uuid: u1.clone(),
            seq: 2,
        });
        kit.cli_forget(im::ForgetTarget::Session { uuid: u2.clone() });

        assert_eq!(
            uuids(&get(&kit, "/annotations").await),
            [other["uuid"].as_str().unwrap()]
        );
        for a in [&on_cmd, &on_s2] {
            let id = a["uuid"].as_str().unwrap();
            assert_eq!(
                patch(&kit, "alice", id, json!({"body": "x"})).await.0,
                StatusCode::NOT_FOUND
            );
            assert_eq!(
                call(&kit, "DELETE", &format!("/annotations/{id}"), "alice", None)
                    .await
                    .0,
                StatusCode::NOT_FOUND
            );
        }
        assert_eq!(
            post(&kit, "alice", json!({"session_uuid": u1, "cmd_seq": 2, "body": "x"}))
                .await
                .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            post(
                &kit,
                "alice",
                json!({"session_uuid": u2, "t_from": 0.0, "t_to": 1.0, "body": "x"})
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn tag_counts_leave_out_forgotten_targets() {
        let kit = Kit::new(8);
        fixture(&kit);
        let (u1, u2) = (uuid(1), uuid(2));
        for (target, tags) in [
            (json!({"session_uuid": u1, "cmd_seq": 2}), json!(["ops", "db"])),
            (json!({"session_uuid": u1, "cmd_seq": 3}), json!(["ops"])),
            (json!({"session_uuid": u2, "t_from": 0.0, "t_to": 1.0}), json!(["db"])),
        ] {
            let mut body = target;
            body["tags"] = tags;
            assert_eq!(post(&kit, "alice", body).await.0, StatusCode::CREATED);
        }
        let counts = |v: Value| -> Vec<(String, u64)> {
            v["tags"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| (t["name"].as_str().unwrap().to_owned(), t["count"].as_u64().unwrap()))
                .collect()
        };
        assert_eq!(
            counts(get(&kit, "/tags").await),
            [("db".to_owned(), 2), ("ops".to_owned(), 2)]
        );
        kit.cli_forget(im::ForgetTarget::Command {
            uuid: u1.clone(),
            seq: 2,
        });
        kit.cli_forget(im::ForgetTarget::Session { uuid: u2.clone() });
        assert_eq!(
            counts(get(&kit, "/tags").await),
            [("db".to_owned(), 0), ("ops".to_owned(), 1)]
        );
    }

    /// A command inside a forgotten range is gone too, before and after the index purge.
    #[tokio::test]
    async fn range_forgotten_commands_are_gone() {
        let kit = Kit::new(8);
        fixture(&kit);
        let u = uuid(1);
        let (_, on_3) = post(&kit, "alice", json!({"session_uuid": u, "cmd_seq": 3, "body": "c3"})).await;
        let (_, on_1) = post(&kit, "alice", json!({"session_uuid": u, "cmd_seq": 1, "body": "c1"})).await;
        let (_, span) = post(
            &kit,
            "alice",
            json!({"session_uuid": u, "t_from": 0.0, "t_to": 100.0, "body": "span"}),
        )
        .await;
        let c3 = {
            let db = IndexDb::open(kit.ctx()).unwrap();
            let id = db.session(&u).unwrap().unwrap().id;
            db.command(id, 3).unwrap().unwrap()
        };
        kit.cli_forget(im::ForgetTarget::Range {
            uuid: u.clone(),
            t_from: c3.t_start,
            t_to: c3.t_end.unwrap(),
        });
        let visible = [on_1["uuid"].as_str().unwrap(), span["uuid"].as_str().unwrap()];
        let id3 = on_3["uuid"].as_str().unwrap();
        for purged in [false, true] {
            if purged {
                kit.purge();
                let db = IndexDb::open(kit.ctx()).unwrap();
                let id = db.session(&u).unwrap().unwrap().id;
                assert!(db.command(id, 3).unwrap().is_none(), "the purge drops the command row");
            }
            assert_eq!(uuids(&get(&kit, "/annotations").await), visible, "purged: {purged}");
            assert_eq!(
                patch(&kit, "alice", id3, json!({"body": "x"})).await.0,
                StatusCode::NOT_FOUND
            );
            assert_eq!(
                post(&kit, "alice", json!({"session_uuid": u, "cmd_seq": 3, "body": "x"}))
                    .await
                    .0,
                StatusCode::NOT_FOUND
            );
        }
        assert_eq!(
            call(&kit, "DELETE", &format!("/annotations/{id3}"), "alice", None)
                .await
                .0,
            StatusCode::NOT_FOUND
        );
    }

    /// A write that took the clock before a racing write committed still stores (and
    /// publishes) a later `updated_at` than the row it overwrites.
    #[tokio::test]
    async fn updated_at_never_goes_back() {
        let kit = Kit::new(8);
        fixture(&kit);
        let (_, a) = post(
            &kit,
            "alice",
            json!({"session_uuid": uuid(1), "cmd_seq": 1, "body": "v1"}),
        )
        .await;
        let id = a["uuid"].as_str().unwrap().to_owned();
        let (ctx, bus) = (kit.ctx(), &kit.state.bus);
        let mut rx = bus.subscribe();
        let edited = update_annotation(
            ctx,
            bus,
            &id,
            &serde_json::from_value(json!({"body": "v2"})).unwrap(),
            "alice",
            now_us,
        )
        .unwrap();

        let stale = a["updated_at"].as_f64().unwrap() - 60.0;
        let req = serde_json::from_value(json!({"add_tags": ["reviewed"]})).unwrap();
        let tagged = update_annotation(ctx, bus, &id, &req, "bob", || stale).unwrap();
        assert!(tagged.updated_at > edited.updated_at);
        assert_eq!(
            (tagged.body.as_str(), tagged.tags.as_slice()),
            ("v2", ["reviewed".to_owned()].as_slice())
        );
        let deleted = delete_annotation(ctx, bus, &id, "alice", || stale).unwrap();
        assert!(deleted.updated_at > tagged.updated_at);
        assert_eq!(deleted.deleted_at, Some(deleted.updated_at));

        let stored = cinelog_index::db::open_meta(&ctx.db)
            .unwrap()
            .annotation(&id)
            .unwrap()
            .unwrap();
        assert_eq!(stored.updated_at, deleted.updated_at);
        let published: Vec<f64> = drain(&mut rx)
            .into_iter()
            .filter_map(|e| match e {
                BusEvent::Annotation(e) => Some(e.annotation.updated_at),
                _ => None,
            })
            .collect();
        assert_eq!(published, [edited.updated_at, tagged.updated_at, deleted.updated_at]);
        assert_eq!(next_update(5.0, 7.0), 7.0);
        assert_eq!(next_update(5.0, 5.0), 5.000001);
        assert_eq!(next_update(1_700_000_000.25, 1.0), 1_700_000_000.250001);
    }

    #[tokio::test]
    async fn without_an_index() {
        let kit = Kit::new(8);
        let (status, err) = post(
            &kit,
            "alice",
            json!({"session_uuid": uuid(1), "cmd_seq": 1, "body": "x"}),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(err["error"], "unavailable");
        assert_eq!(
            get(&kit, "/annotations").await,
            json!({"annotations": [], "next": null})
        );
        assert_eq!(get(&kit, "/tags").await, json!({"tags": []}));
    }

    /// Accept 3 of M4: annotations survive `index --rebuild` and a meta backup restore.
    #[tokio::test]
    async fn survive_a_rebuild_and_a_backup_restore() {
        let kit = Kit::new(8);
        let casts = fixture(&kit);
        let u = uuid(1);
        post(
            &kit,
            "alice",
            json!({"session_uuid": u, "cmd_seq": 2, "body": "keep me", "tags": ["k"]}),
        )
        .await;
        post(
            &kit,
            "bob",
            json!({"session_uuid": u, "t_from": 1.0, "t_to": 2.0, "body": "span"}),
        )
        .await;
        call(
            &kit,
            "POST",
            "/tags",
            "alice",
            Some(json!({"name": "k", "color": "#123456"})),
        )
        .await;
        let before = get(&kit, &format!("/annotations?session={u}")).await;
        let tags_before = get(&kit, "/tags").await;
        assert_eq!(before["annotations"].as_array().unwrap().len(), 2);

        // Rebuild: index.db is dropped and the casts are indexed again.
        let db = &kit.ctx().db;
        for p in [
            &db.index,
            &db.index.with_extension("db-wal"),
            &db.index.with_extension("db-shm"),
        ] {
            let _ = std::fs::remove_file(p);
        }
        for cast in &casts {
            kit.index(cast, None);
        }
        assert_eq!(get(&kit, &format!("/annotations?session={u}")).await, before);
        assert_eq!(
            post(
                &kit,
                "alice",
                json!({"session_uuid": u, "cmd_seq": 3, "body": "after rebuild"})
            )
            .await
            .0,
            StatusCode::CREATED
        );
        let all = get(&kit, &format!("/annotations?session={u}")).await;

        // Backup and restore into a fresh meta.db (the nightly JSONL and `meta import`).
        let mut backup = Vec::new();
        cinelog_index::db::open_meta(db)
            .unwrap()
            .export_jsonl(&mut backup)
            .unwrap();
        for p in [
            &db.meta,
            &db.meta.with_extension("db-wal"),
            &db.meta.with_extension("db-shm"),
        ] {
            let _ = std::fs::remove_file(p);
        }
        assert!(uuids(&get(&kit, "/annotations").await).is_empty());
        cinelog_index::db::open_meta(db)
            .unwrap()
            .import_jsonl(&mut backup.as_slice())
            .unwrap();
        assert_eq!(get(&kit, &format!("/annotations?session={u}")).await, all);
        assert_eq!(get(&kit, "/tags").await["tags"][0], tags_before["tags"][0]);
    }

    #[test]
    fn cursors_round_trip() {
        let c = Cursor {
            created_at: 1_731_070_000.123_456,
            uuid: "0193a1b2-0000-7000-8000-000000000001".into(),
        };
        assert_eq!(Cursor::decode(&c.encode()).unwrap(), c);
        for bad in [
            "",
            "1.5",
            "x~0193a1b2-0000-7000-8000-000000000001",
            "inf~0193a1b2-0000-7000-8000-000000000001",
        ] {
            assert!(Cursor::decode(bad).is_err(), "{bad:?}");
        }
    }
}
