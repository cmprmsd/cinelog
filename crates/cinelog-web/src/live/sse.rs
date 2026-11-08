//! Server-sent events framing and the streamed response (docs/API.md §9.0).
//!
//! Framed by hand rather than with `axum::response::sse`: its events leave out the `data`
//! field when the data is empty, and browsers do not dispatch such an event, but the live
//! stream's final `done` must reach the player with empty data.

use std::convert::Infallible;
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use bytes::Bytes;
use futures_util::{Stream, StreamExt};

use crate::api::types::{BusEvent, LiveMessage};
use crate::bus::Bus;

/// A comment frame this often keeps proxies and browsers from timing out.
pub(crate) const KEEP_ALIVE: Duration = Duration::from_secs(15);

const KEEP_ALIVE_FRAME: &[u8] = b": keep-alive\n\n";

/// An unnamed (`message`) event. `data` is one line of JSON.
pub(crate) fn message(data: &str) -> Bytes {
    named_frame(None, data)
}

/// One `LiveMessage` as a `message` event.
pub(crate) fn live_message(msg: &LiveMessage) -> Bytes {
    let json = serde_json::to_string(msg).unwrap_or_else(|_| unreachable!("live messages serialise"));
    message(&json)
}

/// The live stream's last event: `done` with empty data.
pub(crate) fn done() -> Bytes {
    named_frame(Some("done"), "")
}

/// A bus event: the SSE event name is its type, the data its payload.
pub(crate) fn bus_event(ev: &BusEvent) -> Bytes {
    named_frame(Some(ev.name()), &ev.data_json())
}

fn named_frame(name: Option<&str>, data: &str) -> Bytes {
    let mut out = Vec::with_capacity(data.len() + 32);
    if let Some(name) = name {
        out.extend_from_slice(b"event: ");
        out.extend_from_slice(name.as_bytes());
        out.push(b'\n');
    }
    // JSON never contains a raw line break; anything else is split into data lines.
    for line in data.split(['\n', '\r']) {
        push_data(&mut out, line);
    }
    out.push(b'\n');
    Bytes::from(out)
}

fn push_data(out: &mut Vec<u8>, line: &str) {
    if line.is_empty() {
        out.extend_from_slice(b"data:\n");
    } else {
        out.extend_from_slice(b"data: ");
        out.extend_from_slice(line.as_bytes());
        out.push(b'\n');
    }
}

/// A `text/event-stream` response of `frames` with keep-alive comments. It ends with
/// `frames`, or when `bus` closes (server shutdown). `guard` lives as long as the body.
pub(crate) fn response<S, G>(frames: S, bus: Bus, guard: G) -> Response
where
    S: Stream<Item = Bytes> + Send + 'static,
    G: Send + 'static,
{
    response_every(frames, bus, guard, KEEP_ALIVE)
}

fn response_every<S, G>(frames: S, bus: Bus, guard: G, keep_alive_every: Duration) -> Response
where
    S: Stream<Item = Bytes> + Send + 'static,
    G: Send + 'static,
{
    let body = async_stream::stream! {
        let _guard = guard;
        let mut frames = std::pin::pin!(frames);
        let closed = bus.closed();
        let mut closed = std::pin::pin!(closed);
        let mut keep_alive =
            tokio::time::interval_at(tokio::time::Instant::now() + keep_alive_every, keep_alive_every);
        keep_alive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                () = &mut closed => break,
                frame = frames.next() => match frame {
                    Some(f) => yield Ok::<Bytes, Infallible>(f),
                    None => break,
                },
                _ = keep_alive.tick() => yield Ok(Bytes::from_static(KEEP_ALIVE_FRAME)),
            }
        }
    };
    let mut resp = Response::new(Body::from_stream(body));
    *resp.status_mut() = StatusCode::OK;
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert("x-accel-buffering", HeaderValue::from_static("no"));
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::types::{LiveEvent, LiveOffline, LiveStreamState, Tag};

    fn text(b: Bytes) -> String {
        String::from_utf8(b.to_vec()).unwrap()
    }

    #[test]
    fn frames() {
        assert_eq!(text(message(r#"{"a":1}"#)), "data: {\"a\":1}\n\n");
        assert_eq!(text(done()), "event: done\ndata:\n\n");
        assert_eq!(text(named_frame(None, "a\nb")), "data: a\ndata: b\n\n");
        let ev = BusEvent::Tag(Tag {
            name: "x".into(),
            color: None,
            count: 2,
        });
        assert_eq!(
            text(bus_event(&ev)),
            "event: tag\ndata: {\"name\":\"x\",\"color\":null,\"count\":2}\n\n"
        );
        assert_eq!(
            text(live_message(&LiveMessage::Event(LiveEvent(
                1.5,
                "o".into(),
                "a\r\n".into()
            )))),
            "data: [1.5,\"o\",\"a\\r\\n\"]\n\n"
        );
        assert_eq!(
            text(live_message(&LiveMessage::Offline(LiveOffline {
                state: LiveStreamState::Offline
            }))),
            "data: {\"state\":\"offline\"}\n\n"
        );
    }

    #[tokio::test]
    async fn keep_alive_and_close() {
        use http_body_util::BodyExt;
        let bus = Bus::new();
        let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(4);
        let resp = response_every(
            tokio_stream::wrappers::ReceiverStream::new(rx),
            bus.clone(),
            (),
            Duration::from_millis(50),
        );
        assert_eq!(resp.headers()[header::CONTENT_TYPE], "text/event-stream");
        assert_eq!(resp.headers()["x-accel-buffering"], "no");
        let mut body = resp.into_body();
        tx.send(message("1")).await.unwrap();
        let f = body.frame().await.unwrap().unwrap().into_data().unwrap();
        assert_eq!(f, message("1"));
        // Nothing to send for a while: a keep-alive comment.
        let f = body.frame().await.unwrap().unwrap().into_data().unwrap();
        assert_eq!(&f[..], KEEP_ALIVE_FRAME);
        bus.close();
        assert!(body.frame().await.is_none(), "shutdown ends the stream");
        drop(tx);
    }
}
