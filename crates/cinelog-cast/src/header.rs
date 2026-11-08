//! Header parsing: asciicast v3, and v2 normalised to the v3 shape.

use serde_json::{Map, Value};

use crate::{CAST_VERSION, CastError, Format, Header};

/// Parses a header line (v2 or v3).
///
/// A v2 header is normalised: `width`/`height` become `term.cols`/`term.rows`, `theme` moves
/// to `term.theme`, `env.TERM` fills `term.type`, `version` becomes 3 and v2-only keys such
/// as `duration` land in `extra`. Tolerances for both versions: a float `timestamp` is
/// truncated, non-string `env` values (v2 writes `null`) are dropped.
pub fn parse_header(line: &[u8]) -> Result<(Header, Format), CastError> {
    let line = line.strip_prefix(b"\xef\xbb\xbf").unwrap_or(line);
    let value: Value =
        serde_json::from_slice(line.trim_ascii()).map_err(|e| CastError::Header(format!("not JSON: {e}")))?;
    let Value::Object(mut map) = value else {
        return Err(CastError::Header("not a JSON object".into()));
    };
    let version = map.get("version").and_then(Value::as_u64);
    let format = match version {
        Some(3) => Format::V3,
        Some(2) => {
            map = v2_to_v3(map);
            Format::V2
        }
        Some(v) => return Err(CastError::Header(format!("unsupported asciicast version {v}"))),
        None => return Err(CastError::Header("missing version".into())),
    };
    normalise(&mut map);
    let header: Header =
        serde_json::from_value(Value::Object(map)).map_err(|e| CastError::Header(format!("invalid header: {e}")))?;
    Ok((header, format))
}

fn v2_to_v3(mut v2: Map<String, Value>) -> Map<String, Value> {
    let mut term = Map::new();
    if let Some(w) = v2.remove("width") {
        term.insert("cols".into(), w);
    }
    if let Some(h) = v2.remove("height") {
        term.insert("rows".into(), h);
    }
    let term_type = v2
        .get("env")
        .and_then(|e| e.get("TERM"))
        .filter(|t| t.is_string())
        .cloned();
    if let Some(t) = term_type {
        term.insert("type".into(), t);
    }
    if let Some(theme) = v2.remove("theme").filter(|t| !t.is_null()) {
        term.insert("theme".into(), theme);
    }
    v2.remove("version");
    let mut out = Map::new();
    out.insert("version".into(), Value::from(CAST_VERSION));
    out.insert("term".into(), Value::Object(term));
    out.extend(v2);
    out
}

fn normalise(map: &mut Map<String, Value>) {
    if let Some(ts) = map.get_mut("timestamp")
        && let Some(f) = ts.as_f64()
        && !ts.is_i64()
    {
        *ts = Value::from(f.trunc() as i64);
    }
    if let Some(Value::Object(env)) = map.get_mut("env") {
        env.retain(|_, v| v.is_string());
    } else if map.get("env").is_some_and(|e| !e.is_object()) {
        map.remove("env");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v3_header() {
        let (h, f) = parse_header(
            br#"{"version":3,"term":{"cols":120,"rows":40,"type":"xterm-256color","x":1},"timestamp":1731070000,"env":{"SHELL":"/bin/zsh"},"foo":"bar"}"#,
        )
        .unwrap();
        assert_eq!(f, Format::V3);
        assert_eq!((h.term.cols, h.term.rows), (120, 40));
        assert_eq!(h.term.term_type.as_deref(), Some("xterm-256color"));
        assert_eq!(h.term.extra.get("x"), Some(&Value::from(1)));
        assert_eq!(h.timestamp, Some(1_731_070_000));
        assert_eq!(h.extra.get("foo"), Some(&Value::from("bar")));
    }

    #[test]
    fn v2_header_is_normalised() {
        let (h, f) = parse_header(
            br##"{"version": 2, "width": 80, "height": 24, "timestamp": 1504467315.5, "duration": 4.1, "idle_time_limit": 2.0, "title": null, "env": {"SHELL": "/bin/bash", "TERM": "xterm", "LANG": null}, "theme": {"fg": "#fff"}}"##,
        )
        .unwrap();
        assert_eq!(f, Format::V2);
        assert_eq!(h.version, CAST_VERSION);
        assert_eq!((h.term.cols, h.term.rows), (80, 24));
        assert_eq!(h.term.term_type.as_deref(), Some("xterm"));
        assert_eq!(h.term.theme, Some(serde_json::json!({"fg": "#fff"})));
        assert_eq!(h.timestamp, Some(1_504_467_315));
        assert_eq!(h.idle_time_limit, Some(2.0));
        assert_eq!(h.title, None);
        assert_eq!(h.env.as_ref().map(|e| e.len()), Some(2));
        assert_eq!(h.extra.get("duration"), Some(&Value::from(4.1)));
    }

    #[test]
    fn bom_and_whitespace() {
        let (h, _) = parse_header(b"\xef\xbb\xbf {\"version\":3,\"term\":{\"cols\":1,\"rows\":2}} \r").unwrap();
        assert_eq!((h.term.cols, h.term.rows), (1, 2));
    }

    #[test]
    fn rejects_bad_headers() {
        for bad in [
            &b"not json"[..],
            b"[1,2]",
            br#"{"version":1,"stdout":[]}"#,
            br#"{"term":{"cols":1,"rows":1}}"#,
            br#"{"version":3}"#,
            br#"{"version":2,"width":80}"#,
        ] {
            assert!(
                matches!(parse_header(bad), Err(CastError::Header(_))),
                "{:?}",
                String::from_utf8_lossy(bad)
            );
        }
    }
}
