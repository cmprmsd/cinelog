//! A small, tolerant asciicast reader written from docs/PROTOCOL.md §5, independent of the
//! cinelog-cast crate so the tests check the file format rather than the implementation.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use regex::Regex;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    /// Position in the cast's event list.
    pub index: usize,
    /// Seconds since t=0 (sum of the v3 intervals).
    pub time: f64,
    pub code: String,
    pub data: String,
}

/// A cinelog `m` event (data is JSON with `"cl":1`).
#[derive(Debug, Clone, PartialEq)]
pub struct Marker {
    pub index: usize,
    pub time: f64,
    pub kind: String,
    pub json: Value,
}

impl Marker {
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.json.get(key)
    }

    pub fn str(&self, key: &str) -> Option<&str> {
        self.json.get(key)?.as_str()
    }

    pub fn u64(&self, key: &str) -> Option<u64> {
        self.json.get(key)?.as_u64()
    }

    pub fn i64(&self, key: &str) -> Option<i64> {
        self.json.get(key)?.as_i64()
    }

    pub fn f64(&self, key: &str) -> Option<f64> {
        self.json.get(key)?.as_f64()
    }

    pub fn flag(&self, key: &str) -> bool {
        self.json.get(key).and_then(Value::as_bool).unwrap_or(false)
    }
}

/// One `cmd` with its position in the nesting derived like the indexer's segmenter (§6.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CmdNode {
    pub seq: u64,
    pub sh: u64,
    pub n: u64,
    pub typed: String,
    pub parent_seq: Option<u64>,
    pub depth: usize,
    pub exit: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct Cast {
    pub path: PathBuf,
    pub header: Value,
    pub events: Vec<Event>,
    /// Lines that did not parse (a truncated last line is normal while a cast is live).
    pub bad_lines: usize,
}

impl Cast {
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(path, &bytes)
    }

    pub fn parse(path: &Path, bytes: &[u8]) -> Result<Self> {
        let text = String::from_utf8_lossy(bytes);
        let mut lines = text.split('\n');
        let first = lines.next().unwrap_or_default();
        let header: Value = serde_json::from_str(first).with_context(|| format!("header of {}", path.display()))?;
        let absolute = match header.get("version").and_then(Value::as_u64) {
            Some(3) => false,
            Some(2) => true,
            other => bail!("{}: unsupported cast version {other:?}", path.display()),
        };
        let mut events = Vec::new();
        let mut bad_lines = 0;
        let mut time = 0.0;
        for line in lines.filter(|l| !l.trim().is_empty()) {
            let Some((t, code, data)) = parse_event(line) else {
                bad_lines += 1;
                continue;
            };
            time = if absolute { t } else { time + t };
            events.push(Event {
                index: events.len(),
                time,
                code,
                data,
            });
        }
        Ok(Self {
            path: path.to_path_buf(),
            header,
            events,
            bad_lines,
        })
    }

    pub fn markers(&self) -> Vec<Marker> {
        self.events.iter().filter_map(marker_of).collect()
    }

    pub fn markers_of(&self, kind: &str) -> Vec<Marker> {
        self.markers().into_iter().filter(|m| m.kind == kind).collect()
    }

    pub fn first_marker(&self, kind: &str) -> Option<Marker> {
        self.events.iter().filter_map(marker_of).find(|m| m.kind == kind)
    }

    /// The `session` marker at t=0.
    pub fn session(&self) -> Option<Marker> {
        self.events.first().and_then(marker_of).filter(|m| m.kind == "session")
    }

    pub fn uuid(&self) -> Option<String> {
        self.session()?.str("uuid").map(str::to_owned)
    }

    pub fn rec_pid(&self) -> Option<u32> {
        self.session()?.u64("rec_pid").and_then(|p| u32::try_from(p).ok())
    }

    pub fn shell_pid(&self) -> Option<u32> {
        self.session()?.u64("shell_pid").and_then(|p| u32::try_from(p).ok())
    }

    /// Concatenated `o` data.
    pub fn output(&self) -> String {
        self.data_of("o", 0, self.events.len())
    }

    /// Concatenated `i` data.
    pub fn input(&self) -> String {
        self.data_of("i", 0, self.events.len())
    }

    /// Concatenated data of `code` events with index in `from..to`.
    pub fn data_of(&self, code: &str, from: usize, to: usize) -> String {
        self.events[from.min(self.events.len())..to.min(self.events.len())]
            .iter()
            .filter(|e| e.code == code)
            .map(|e| e.data.as_str())
            .collect()
    }

    pub fn last_event(&self) -> Option<&Event> {
        self.events.last()
    }

    /// Data of the final `x` event if it is the last event.
    pub fn exit_status(&self) -> Option<&str> {
        self.events.last().filter(|e| e.code == "x").map(|e| e.data.as_str())
    }

    /// The first `cmd` whose `typed` equals `typed`.
    pub fn cmd(&self, typed: &str) -> Option<Marker> {
        self.markers_of("cmd")
            .into_iter()
            .find(|m| m.str("typed") == Some(typed))
    }

    pub fn done_for(&self, seq: u64) -> Option<Marker> {
        self.markers_of("done").into_iter().find(|m| m.u64("seq") == Some(seq))
    }

    /// `(pause, resume)` marker indices; the resume is `None` while still paused.
    pub fn pause_intervals(&self) -> Vec<(usize, Option<usize>)> {
        let mut out = Vec::new();
        let mut open: Option<usize> = None;
        for m in self.markers() {
            match m.kind.as_str() {
                "pause" if open.is_none() => open = Some(m.index),
                "resume" => {
                    if let Some(p) = open.take() {
                        out.push((p, Some(m.index)));
                    }
                }
                _ => {}
            }
        }
        if let Some(p) = open {
            out.push((p, None));
        }
        out
    }

    /// `o`/`i` events recorded while paused (must be empty, PROTOCOL §4.3).
    pub fn events_while_paused(&self) -> Vec<&Event> {
        self.pause_intervals()
            .into_iter()
            .flat_map(|(p, r)| {
                let end = r.unwrap_or(self.events.len());
                self.events[p + 1..end]
                    .iter()
                    .filter(|e| e.code == "o" || e.code == "i")
            })
            .collect()
    }

    /// Whether the recorded output up to and including event `index` leaves the terminal
    /// on the alternate screen.
    pub fn alt_screen_after(&self, index: usize) -> bool {
        alt_screen_state(&self.data_of("o", 0, index + 1))
    }

    /// Commands with derived nesting: a `cmd` from shell B while a `cmd` of shell A is open
    /// gets A's seq as `parent_seq` (the indexer's segmenter stack, §6.4).
    pub fn command_tree(&self) -> Vec<CmdNode> {
        let mut nodes: Vec<CmdNode> = Vec::new();
        let mut stack: Vec<(u64, u64, u64)> = Vec::new();
        for m in self.markers() {
            let (Some(sh), Some(n)) = (m.u64("sh"), m.u64("n")) else {
                continue;
            };
            match m.kind.as_str() {
                "cmd" => {
                    if let Some(pos) = stack.iter().position(|e| e.1 == sh) {
                        stack.truncate(pos);
                    }
                    let seq = m.u64("seq").unwrap_or(0);
                    nodes.push(CmdNode {
                        seq,
                        sh,
                        n,
                        typed: m.str("typed").unwrap_or_default().to_owned(),
                        parent_seq: stack.last().map(|e| e.0),
                        depth: stack.len(),
                        exit: None,
                    });
                    stack.push((seq, sh, n));
                }
                "done" => {
                    if let Some(pos) = stack.iter().position(|e| e.1 == sh && e.2 == n) {
                        let seq = stack[pos].0;
                        stack.truncate(pos);
                        if let Some(node) = nodes.iter_mut().rev().find(|c| c.seq == seq) {
                            node.exit = m.i64("exit");
                        }
                    }
                }
                _ => {}
            }
        }
        nodes
    }
}

fn parse_event(line: &str) -> Option<(f64, String, String)> {
    let v: Value = serde_json::from_str(line).ok()?;
    let a = v.as_array()?;
    Some((
        a.first()?.as_f64()?,
        a.get(1)?.as_str()?.to_owned(),
        a.get(2)?.as_str()?.to_owned(),
    ))
}

fn marker_of(e: &Event) -> Option<Marker> {
    if e.code != "m" {
        return None;
    }
    let json: Value = serde_json::from_str(&e.data).ok()?;
    json.get("cl")?;
    Some(Marker {
        index: e.index,
        time: e.time,
        kind: json.get("k")?.as_str()?.to_owned(),
        json,
    })
}

/// Alternate-screen state after `output` (`?1049`, `?1047`, `?47` set/reset).
pub fn alt_screen_state(output: &str) -> bool {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"\x1b\[\?(?:1049|1047|47)([hl])").expect("valid regex"));
    re.captures_iter(output).last().is_some_and(|c| &c[1] == "h")
}

/// Every `*.cast` file below `dir`, sorted by path.
pub fn find_casts(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk(dir, &mut out);
    out.sort();
    out
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        match e.file_type() {
            Ok(t) if t.is_dir() => walk(&p, out),
            Ok(t) if t.is_file() && p.extension().is_some_and(|x| x == "cast") => out.push(p),
            _ => {}
        }
    }
}

/// Whether any file below `dir` contains `needle` as raw bytes.
pub fn any_file_contains(dir: &Path, needle: &[u8]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    walk_all(dir, &mut files);
    files
        .into_iter()
        .filter(|p| fs::read(p).is_ok_and(|b| memmem(&b, needle)))
        .collect()
}

fn walk_all(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        match e.file_type() {
            Ok(t) if t.is_dir() => walk_all(&e.path(), out),
            Ok(t) if t.is_file() => out.push(e.path()),
            _ => {}
        }
    }
}

pub fn memmem(hay: &[u8], needle: &[u8]) -> bool {
    needle.is_empty() || hay.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAST: &str = concat!(
        r#"{"version":3,"term":{"cols":80,"rows":24,"type":"xterm-256color"},"timestamp":1731070000}"#,
        "\n",
        r#"[0.000000, "m", "{\"cl\":1,\"k\":\"session\",\"uuid\":\"u-1\",\"rec_pid\":42,\"shell_pid\":43}"]"#,
        "\n",
        r#"[0.100000, "o", "$ "]"#,
        "\n",
        r#"[0.200000, "m", "{\"cl\":1,\"k\":\"cmd\",\"seq\":1,\"sh\":43,\"n\":1,\"typed\":\"zsh\"}"]"#,
        "\n",
        r#"[0.100000, "m", "{\"cl\":1,\"k\":\"cmd\",\"seq\":2,\"sh\":50,\"n\":1,\"typed\":\"ls\"}"]"#,
        "\n",
        r#"[0.100000, "m", "{\"cl\":1,\"k\":\"done\",\"seq\":2,\"sh\":50,\"n\":1,\"exit\":0}"]"#,
        "\n",
        r#"[0.100000, "m", "{\"cl\":1,\"k\":\"cmd\",\"seq\":3,\"sh\":50,\"n\":2,\"typed\":\"tmux a\"}"]"#,
        "\n",
        r#"[0.100000, "o", "\u001b[?1049h"]"#,
        "\n",
        r#"[0.100000, "m", "{\"cl\":1,\"k\":\"pause\",\"reason\":\"multiplexer\"}"]"#,
        "\n",
        r#"[0.100000, "r", "100x30"]"#,
        "\n",
        r#"[0.100000, "m", "{\"cl\":1,\"k\":\"resume\",\"reason\":\"foreground\",\"alt_fixup\":true}"]"#,
        "\n",
        r#"[0.000000, "o", "\u001b[?1049l\u001b[0m"]"#,
        "\n",
        r#"[0.100000, "m", "{\"cl\":1,\"k\":\"done\",\"seq\":3,\"sh\":50,\"n\":2,\"exit\":1}"]"#,
        "\n",
        r#"[0.100000, "m", "{\"cl\":1,\"k\":\"done\",\"seq\":1,\"sh\":43,\"n\":1,\"exit\":0}"]"#,
        "\n",
        r#"[0.100000, "m", "foreign label"]"#,
        "\n",
        r#"[0.500000, "x", "0"]"#,
        "\n",
        r#"[0.1, "o", "trunc"#,
    );

    fn cast() -> Cast {
        Cast::parse(Path::new("t.cast"), CAST.as_bytes()).unwrap()
    }

    #[test]
    fn parses_events_markers_and_tolerates_a_truncated_line() {
        let c = cast();
        assert_eq!(c.bad_lines, 1);
        assert_eq!(c.events.len(), 15);
        assert!((c.events[2].time - 0.3).abs() < 1e-9);
        assert_eq!(c.uuid().as_deref(), Some("u-1"));
        assert_eq!((c.rec_pid(), c.shell_pid()), (Some(42), Some(43)));
        assert_eq!(c.markers().len(), 9, "the foreign label is not a cinelog marker");
        assert_eq!(c.exit_status(), Some("0"));
        assert_eq!(c.cmd("ls").unwrap().u64("seq"), Some(2));
        assert_eq!(c.done_for(3).unwrap().i64("exit"), Some(1));
        assert!(c.output().starts_with("$ "));
    }

    #[test]
    fn pause_intervals_and_alt_screen() {
        let c = cast();
        let iv = c.pause_intervals();
        assert_eq!(iv, vec![(7, Some(9))]);
        assert!(c.events_while_paused().is_empty());
        assert!(c.alt_screen_after(6));
        assert!(c.alt_screen_after(9));
        assert!(!c.alt_screen_after(10));
        assert!(alt_screen_state("\x1b[?47h"));
        assert!(!alt_screen_state("\x1b[?1047h x \x1b[?1047l"));
    }

    #[test]
    fn command_tree_derives_parent_seq() {
        let tree = cast().command_tree();
        let by_seq = |s| tree.iter().find(|n| n.seq == s).unwrap();
        assert_eq!(by_seq(1).parent_seq, None);
        assert_eq!(by_seq(2).parent_seq, Some(1));
        assert_eq!(by_seq(3).parent_seq, Some(1));
        assert_eq!(by_seq(3).depth, 1);
        assert_eq!(by_seq(3).exit, Some(1));
        assert_eq!(by_seq(1).exit, Some(0));
    }

    #[test]
    fn missing_done_closes_the_open_command_of_the_same_shell() {
        let text = concat!(
            r#"{"version":3,"term":{"cols":80,"rows":24}}"#,
            "\n",
            r#"[0, "m", "{\"cl\":1,\"k\":\"cmd\",\"seq\":1,\"sh\":1,\"n\":1,\"typed\":\"a\"}"]"#,
            "\n",
            r#"[0, "m", "{\"cl\":1,\"k\":\"cmd\",\"seq\":2,\"sh\":1,\"n\":2,\"typed\":\"b\"}"]"#,
            "\n",
        );
        let tree = Cast::parse(Path::new("x"), text.as_bytes()).unwrap().command_tree();
        assert_eq!(tree[1].parent_seq, None);
    }

    #[test]
    fn v2_times_are_absolute() {
        let text = "{\"version\":2,\"width\":80,\"height\":24}\n[1.5, \"o\", \"a\"]\n[2.0, \"o\", \"b\"]\n";
        let c = Cast::parse(Path::new("x"), text.as_bytes()).unwrap();
        assert!((c.events[1].time - 2.0).abs() < 1e-9);
        assert!(Cast::parse(Path::new("x"), b"{\"version\":9}\n").is_err());
    }

    #[test]
    fn finds_casts_and_raw_bytes() {
        let d = crate::util::scratch_dir().unwrap();
        fs::create_dir_all(d.path().join("2026-11")).unwrap();
        fs::write(d.path().join("2026-11/a.cast"), "x secret y").unwrap();
        fs::write(d.path().join("b.txt"), "nothing").unwrap();
        assert_eq!(find_casts(d.path()), vec![d.path().join("2026-11/a.cast")]);
        assert_eq!(
            any_file_contains(d.path(), b"secret"),
            vec![d.path().join("2026-11/a.cast")]
        );
        assert!(memmem(b"abc", b""));
    }
}
