//! Frozen protocol v1 types (see `docs/PROTOCOL.md`).
//!
//! This module is the contract between the plugin, the recorder, the CLI and the
//! indexer. After the M1 contracts freeze, changes here are additive only: new
//! optional fields, new kinds, new enum variants marked in PROTOCOL.md.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Marker protocol version carried in the wire prefix, in `"v"` and in cast `"cl"`.
pub const PROTOCOL_VERSION: u32 = 1;
/// OSC number used for cinelog markers.
pub const OSC_NUMBER: &str = "7777";
/// Second OSC field, identifies cinelog markers among other users of OSC 7777.
pub const OSC_TAG: &str = "cinelog";
/// Fixed byte prefix of every marker, up to and including the `;` before the version field.
pub const WIRE_PREFIX: &[u8] = b"\x1b]7777;cinelog;";
/// Maximum length of the encoded `<payload>` field. Longer markers are passed through
/// unchanged and counted as `oversized`.
pub const MAX_ENCODED_PAYLOAD: usize = 64 * 1024;
/// JSON payloads up to this many bytes are sent base64 (`enc=b`), larger ones as escaped JSON (`enc=j`).
pub const BASE64_MAX_JSON: usize = 1536;
/// Cap for `typed` and `expanded` in `cmd` payloads; longer values are cut and flagged `trunc`.
pub const MAX_COMMAND_TEXT: usize = 16 * 1024;

/// Names of the environment variables the recorder passes to the inner shell.
/// The plugin unsets all of them (`unset -m 'CINELOG_*'`) immediately.
pub mod env {
    /// Number of the inherited pipe fd that carries the nonce line.
    pub const NONCE_FD: &str = "CINELOG_NONCE_FD";
    /// Pid of the recorder (must equal the shell's `$PPID`).
    pub const REC_PID: &str = "CINELOG_REC_PID";
    /// Slave tty path (must equal the shell's `$TTY`).
    pub const TTY: &str = "CINELOG_TTY";
    /// Session uuid of the recording.
    pub const SESSION: &str = "CINELOG_SESSION";
    /// Shell options, see [`ShellOpts`](super::ShellOpts).
    pub const OPTS: &str = "CINELOG_OPTS";
    /// Set by the recorder when it fell back to an unrecorded shell: the reason.
    pub const FAILED: &str = "CINELOG_FAILED";
    /// Tty the fallback happened on.
    pub const FAILED_TTY: &str = "CINELOG_FAILED_TTY";
    /// Pid of the recorder that exec'd the unrecorded shell (equals that shell's `$$`).
    pub const FAILED_PID: &str = "CINELOG_FAILED_PID";
    /// Non-root kill switch (ignored at EUID 0).
    pub const DISABLE: &str = "CINELOG_DISABLE";
}

/// Unknown JSON fields, kept verbatim (protocol §4.6).
pub type Extra = Map<String, Value>;

// ------------------------------------------------------------------ wire basics

/// Per-recording 128-bit nonce, written as 32 lowercase hex characters.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Nonce([u8; 16]);

impl Nonce {
    pub const HEX_LEN: usize = 32;

    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// Lowercase hex form as used on the wire and in the registry.
    pub fn to_hex(&self) -> String {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut s = String::with_capacity(Self::HEX_LEN);
        for b in self.0 {
            s.push(DIGITS[usize::from(b >> 4)] as char);
            s.push(DIGITS[usize::from(b & 0xf)] as char);
        }
        s
    }

    /// Parses exactly 32 lowercase hex characters (uppercase is rejected, as on the wire).
    pub fn parse_hex(s: &[u8]) -> Option<Self> {
        fn nibble(c: u8) -> Option<u8> {
            match c {
                b'0'..=b'9' => Some(c - b'0'),
                b'a'..=b'f' => Some(c - b'a' + 10),
                _ => None,
            }
        }
        if s.len() != Self::HEX_LEN {
            return None;
        }
        let mut out = [0u8; 16];
        for (i, pair) in s.chunks_exact(2).enumerate() {
            out[i] = (nibble(pair[0])? << 4) | nibble(pair[1])?;
        }
        Some(Self(out))
    }
}

impl fmt::Debug for Nonce {
    // Never print the nonce itself: it must not leak into logs.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Nonce(<redacted>)")
    }
}

impl FromStr for Nonce {
    type Err = ParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse_hex(s.as_bytes()).ok_or(ParseError::Nonce)
    }
}

impl Serialize for Nonce {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for Nonce {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// The `<nonce>` field of a marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NonceField {
    /// A session nonce (plugin and `cinelog pause/resume` messages).
    Session(Nonce),
    /// `-`: recorder-to-recorder messages (`nested-start`/`nested-end`), accepted only after
    /// process-tree corroboration.
    Recorder,
}

/// The `<enc>` field of a marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Encoding {
    /// `b`: base64 (standard alphabet, no padding) of UTF-8 JSON.
    Base64,
    /// `j`: UTF-8 JSON with every C0 byte and DEL `\u00XX`-escaped.
    Json,
}

impl Encoding {
    pub const fn as_byte(self) -> u8 {
        match self {
            Self::Base64 => b'b',
            Self::Json => b'j',
        }
    }

    pub const fn from_byte(b: u8) -> Option<Self> {
        match b {
            b'b' => Some(Self::Base64),
            b'j' => Some(Self::Json),
            _ => None,
        }
    }

    /// Encoding the emitter must choose for a JSON document of `json_len` bytes.
    pub const fn for_json_len(json_len: usize) -> Self {
        if json_len <= BASE64_MAX_JSON {
            Self::Base64
        } else {
            Self::Json
        }
    }
}

/// One marker found by a [`MarkerScanner`], not yet decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScannedMarker {
    /// Position in the `out` buffer passed to [`MarkerScanner::feed`] at which the marker
    /// occurred: `out.len()` right before the marker bytes would have been appended (or
    /// were appended, when not stripping). Counts bytes `out` already contained.
    pub at: usize,
    /// Value of the version field (`1` for this protocol).
    pub version: u32,
    pub nonce: NonceField,
    pub enc: Encoding,
    /// Encoded payload bytes (between the last `;` and the terminator).
    pub payload: Vec<u8>,
}

/// Counters kept by a scanner. Every rejected marker passes through to `out` unchanged.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanStats {
    /// Accepted markers (session nonce matched, or `-`).
    pub accepted: u64,
    /// Well-formed markers whose session nonce did not match.
    pub bad_nonce: u64,
    /// Payload longer than [`MAX_ENCODED_PAYLOAD`].
    pub oversized: u64,
    /// Bad version/nonce/enc field syntax.
    pub malformed: u64,
    /// Version field other than [`PROTOCOL_VERSION`].
    pub unsupported_version: u64,
    /// Sequences aborted by CAN/SUB.
    pub aborted: u64,
}

impl ScanStats {
    /// Value stored as `bad_markers` per session.
    pub fn bad_markers(&self) -> u64 {
        self.bad_nonce + self.oversized + self.malformed + self.unsupported_version + self.aborted
    }
}

/// Streaming marker scanner over the PTY output stream (protocol §4.4).
///
/// Implementations must give identical results for any split of the input into chunks,
/// hold back only a possible partial prefix or an unterminated marker, never block, and
/// treat CAN (0x18) and SUB (0x1a) as aborting the OSC. Accepted markers are removed from
/// `out` when stripping is enabled and kept byte-identical otherwise.
pub trait MarkerScanner {
    /// Feeds bytes read from the master. Passthrough bytes are appended to `out`, accepted
    /// markers to `markers` (in stream order).
    fn feed(&mut self, input: &[u8], out: &mut Vec<u8>, markers: &mut Vec<ScannedMarker>);
    /// End of stream: releases any held-back bytes to `out` unchanged.
    fn finish(&mut self, out: &mut Vec<u8>);
    fn stats(&self) -> ScanStats;
}

// ------------------------------------------------------------------ shared enums

/// Recording scope (§5.6): EUID 0 is TEAM, everything else PERSONAL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Team,
    Personal,
}

impl Scope {
    pub const fn for_euid(euid: u32) -> Self {
        if euid == 0 { Self::Team } else { Self::Personal }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Team => "team",
            Self::Personal => "personal",
        }
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Scope {
    type Err = ParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "team" => Ok(Self::Team),
            "personal" => Ok(Self::Personal),
            _ => Err(ParseError::Scope(s.to_owned())),
        }
    }
}

/// How the session's principal was determined (§5.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Attribution {
    /// First ancestor with real uid ≠ 0.
    Ancestor,
    /// `/proc/self/loginuid`.
    Loginuid,
    /// Shared root tmux pane whose creating client could not be resolved: server's loginuid.
    Inherited,
    /// Nothing found; principal is `root` with `login_via`.
    None,
    /// Imported legacy cast.
    Legacy,
}

/// `record.nested` setting and the mode recorded in `nested` link markers (§5.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NestedMode {
    #[default]
    Pause,
    Record,
    Ignore,
}

// ------------------------------------------------------------------ wire payloads (§4.2)

/// tmux hints from `$TMUX` (`socket,server_pid,session_id`) and `$TMUX_PANE`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct TmuxHint {
    #[serde(default)]
    pub socket: String,
    #[serde(default)]
    pub server_pid: u32,
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub pane: String,
}

/// `hello`: once per shell instance, at its first precmd.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    /// Shell pid.
    pub sh: u32,
    /// Shell starttime (field 22 of `/proc/self/stat`, clock ticks since boot).
    #[serde(default)]
    pub sst: u64,
    #[serde(default)]
    pub ppid: u32,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub uid: u32,
    #[serde(default)]
    pub euid: u32,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub tty: String,
    #[serde(default)]
    pub cwd: String,
    /// `$ZSH_VERSION`.
    #[serde(default)]
    pub zsh: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tmux: Option<TmuxHint>,
    /// `$EPOCHREALTIME`.
    #[serde(default)]
    pub t: f64,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `cmd`: emitted by preexec for each command that does not start with a space.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cmd {
    pub sh: u32,
    /// Shell-local counter, starts at 1 in every shell.
    pub n: u64,
    /// `$1` of preexec: the line as typed.
    pub typed: String,
    /// `$3` of preexec: the expanded command.
    #[serde(default)]
    pub expanded: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub user: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub euid: Option<u32>,
    #[serde(default)]
    pub host: String,
    /// `$TMUX_PANE`, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<String>,
    #[serde(default)]
    pub t: f64,
    /// `typed` or `expanded` was cut at [`MAX_COMMAND_TEXT`].
    #[serde(default, skip_serializing_if = "is_false")]
    pub trunc: bool,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `done`: emitted by precmd after a `cmd` of the same shell.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Done {
    pub sh: u32,
    pub n: u64,
    pub exit: i32,
    #[serde(default)]
    pub pipestatus: Vec<i32>,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub t: f64,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `skip`: a leading-space command ran. Carries no command text; leaves a visible
/// "hidden command" marker in the timeline (§15.2, §15.9).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Skip {
    pub sh: u32,
    #[serde(default)]
    pub t: f64,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `user-pause` / `user-resume`, sent by `cinelog pause` / `cinelog resume`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct PauseRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Pid of the requesting `cinelog` process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `nested-start` / `nested-end`, sent by an inner recorder with nonce `-`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NestedInfo {
    /// Session uuid of the inner recording.
    pub uuid: String,
    /// Pid of the inner recorder.
    pub pid: u32,
    /// Starttime of the inner recorder (`/proc/<pid>/stat` field 22).
    pub starttime: u64,
    pub scope: Scope,
    #[serde(default)]
    pub user: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sudo_user: Option<String>,
    /// Web URL of the inner session, once a server for its scope exists (M3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// `nested-end` only: exit status of the inner shell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<i32>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// A decoded wire payload (`{"v":1,"k":…}`).
#[derive(Debug, Clone, PartialEq)]
pub enum WireMessage {
    Hello(Hello),
    Cmd(Cmd),
    Done(Done),
    Skip(Skip),
    UserPause(PauseRequest),
    UserResume(PauseRequest),
    NestedStart(NestedInfo),
    NestedEnd(NestedInfo),
    /// Unknown `k`: ignored by the recorder (protocol §4.6).
    Unknown {
        k: String,
        fields: Extra,
    },
}

impl WireMessage {
    pub fn kind(&self) -> &str {
        match self {
            Self::Hello(_) => "hello",
            Self::Cmd(_) => "cmd",
            Self::Done(_) => "done",
            Self::Skip(_) => "skip",
            Self::UserPause(_) => "user-pause",
            Self::UserResume(_) => "user-resume",
            Self::NestedStart(_) => "nested-start",
            Self::NestedEnd(_) => "nested-end",
            Self::Unknown { k, .. } => k,
        }
    }

    /// Kinds that are valid only with the recorder nonce `-`; all others need the session nonce.
    pub fn requires_recorder_nonce(&self) -> bool {
        matches!(self, Self::NestedStart(_) | Self::NestedEnd(_))
    }

    /// Parses decoded payload JSON. Fails on missing/foreign `v` major, missing `k`,
    /// or a known `k` with missing required fields. Unknown fields are kept in `extra`.
    pub fn from_json(json: &[u8]) -> Result<Self, PayloadError> {
        let mut map: Map<String, Value> = serde_json::from_slice(json).map_err(PayloadError::json)?;
        let version = map
            .remove("v")
            .and_then(|v| v.as_u64())
            .ok_or(PayloadError::MissingVersion)?;
        if version != u64::from(PROTOCOL_VERSION) {
            return Err(PayloadError::UnsupportedVersion(version));
        }
        let k = take_kind(&mut map)?;
        let obj = Value::Object(map);
        Ok(match k.as_str() {
            "hello" => Self::Hello(from_obj(obj)?),
            "cmd" => Self::Cmd(from_obj(obj)?),
            "done" => Self::Done(from_obj(obj)?),
            "skip" => Self::Skip(from_obj(obj)?),
            "user-pause" => Self::UserPause(from_obj(obj)?),
            "user-resume" => Self::UserResume(from_obj(obj)?),
            "nested-start" => Self::NestedStart(from_obj(obj)?),
            "nested-end" => Self::NestedEnd(from_obj(obj)?),
            _ => Self::Unknown {
                k,
                fields: into_map(obj),
            },
        })
    }

    /// Compact JSON with `v` first and `k` second (the form emitters encode).
    pub fn to_json(&self) -> Vec<u8> {
        let body = match self {
            Self::Hello(p) => to_obj(p),
            Self::Cmd(p) => to_obj(p),
            Self::Done(p) => to_obj(p),
            Self::Skip(p) => to_obj(p),
            Self::UserPause(p) | Self::UserResume(p) => to_obj(p),
            Self::NestedStart(p) | Self::NestedEnd(p) => to_obj(p),
            Self::Unknown { fields, .. } => fields.clone(),
        };
        envelope("v", self.kind(), body)
    }
}

// ------------------------------------------------------------------ cast `m` events (§4.5)

/// tmux identity of a pane recording. `(server_pid, server_start, pane)` survives pane-id reuse.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct TmuxIdentity {
    #[serde(default)]
    pub socket: String,
    #[serde(default)]
    pub server_pid: u32,
    /// tmux server starttime (`/proc/<pid>/stat` field 22).
    #[serde(default)]
    pub server_start: u64,
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub pane: String,
}

/// `session` at t=0 of every cast (and of every rotation part).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionInfo {
    /// Session uuid (UUIDv7); identical for all parts of one session.
    pub uuid: String,
    /// 1 for the first cast, N for `…_partN.cast`.
    #[serde(default = "one")]
    pub part: u32,
    /// File name of the previous part, for parts ≥ 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continues: Option<String>,
    /// Wall clock at t=0, µs since the Unix epoch (UTC).
    pub start_epoch_us: i64,
    /// `CLOCK_BOOTTIME` at t=0, µs.
    #[serde(default)]
    pub boottime_us: u64,
    /// IANA zone name, or `"UTC"`/`"unknown"`.
    #[serde(default)]
    pub tz: String,
    /// UTC offset at t=0, seconds east.
    #[serde(default)]
    pub tz_offset_s: i32,
    pub scope: Scope,
    #[serde(default)]
    pub host: String,
    /// Principal: the euid's user name (PERSONAL) or the attributed user (TEAM).
    pub user: String,
    #[serde(default)]
    pub uid: u32,
    #[serde(default)]
    pub euid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sudo_user: Option<String>,
    /// `$SUDO_USER` as found, never used for paths.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_user: Option<String>,
    pub attribution: Attribution,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login_via: Option<String>,
    /// Slave tty of the host side (`/dev/pts/N`, `/dev/ttyN`).
    #[serde(default)]
    pub tty: String,
    /// comm of the PTY master holder (`tmux: server`, `sshd-session`, `terminator`, `sudo`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holder_comm: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holder_pid: Option<u32>,
    #[serde(default)]
    pub rec_pid: u32,
    #[serde(default)]
    pub rec_starttime: u64,
    #[serde(default)]
    pub boot_id: String,
    #[serde(default)]
    pub shell_pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tmux: Option<TmuxIdentity>,
    /// Session that this one is nested in (the outer recorder's uuid), if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_uuid: Option<String>,
    /// Whether `i` events are recorded at all (`record.input`).
    #[serde(default = "yes")]
    pub input: bool,
    #[serde(default)]
    pub rec_version: String,
    #[serde(default = "protocol_version")]
    pub protocol: u32,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClockReason {
    #[default]
    Periodic,
    /// `|Δrealtime − Δboottime| > 2 s` since the last marker.
    Step,
}

/// `clock`: every 5 min and on clock steps (§5.4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClockSync {
    pub epoch_us: i64,
    pub boottime_us: u64,
    #[serde(default)]
    pub reason: ClockReason,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `cmd` in the cast: the wire payload plus the recorder-assigned `seq`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CmdRecord {
    /// Monotonic per cast (across rotation parts), starting at 1.
    pub seq: u64,
    #[serde(flatten)]
    pub cmd: Cmd,
}

/// `done` in the cast; `seq` of the matching `cmd` by `(sh, n)`, `None` if unmatched.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DoneRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    #[serde(flatten)]
    pub done: Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PauseReason {
    /// A multiplexer client (`record.pause_on_multiplexer`) is in the foreground.
    Multiplexer,
    /// A corroborated nested recorder runs in the foreground (`record.nested = pause`).
    Nested,
    /// `cinelog pause`.
    User,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ResumeReason {
    NestedEnd,
    /// The foreground pgrp returned to the shell with no multiplexer left.
    Foreground,
    User,
}

/// `pause`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PauseInfo {
    pub reason: PauseReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `resume`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResumeInfo {
    pub reason: ResumeReason,
    /// A synthetic `ESC[?1049l ESC[0m` `o` event follows (alt screen left open while paused).
    #[serde(default, skip_serializing_if = "is_false")]
    pub alt_fixup: bool,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `nested`: link to the inner session of a corroborated nested recorder.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NestedLink {
    pub child_uuid: String,
    pub scope: Scope,
    #[serde(default)]
    pub user: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sudo_user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default)]
    pub pid: u32,
    #[serde(default)]
    pub starttime: u64,
    /// `pause` or `record` (with `ignore` no marker is written).
    pub mode: NestedMode,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `multiplexer`: the multiplexer client that caused a `pause{reason:multiplexer}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MultiplexerInfo {
    pub comm: String,
    pub pid: u32,
    #[serde(default)]
    pub pgrp: u32,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `ctx`: tmux context of a pane recording, written at start and on change (§5.8).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct TmuxContext {
    #[serde(default)]
    pub session_name: String,
    #[serde(default)]
    pub window_index: String,
    #[serde(default)]
    pub window_name: String,
    #[serde(default)]
    pub pane_index: String,
    #[serde(default)]
    pub pane_id: String,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `gap`: events dropped because the writer queue was full (§5.8).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GapInfo {
    pub dropped_bytes: u64,
    /// Cast offsets in seconds.
    pub from_t: f64,
    pub to_t: f64,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RotateReason {
    /// `record.rotate_mb` reached.
    Size,
    /// `record.rotate_days` reached.
    Age,
    /// `cinelog resume` after degraded mode.
    Resume,
}

/// `rotate`: last event of a part, pointing at the next one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RotateInfo {
    /// The part that ends here.
    pub part: u32,
    /// File name of the next part.
    pub next: String,
    pub reason: RotateReason,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `error`: the writer hit an I/O error; with `degraded` the recorder continues passthrough-only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorInfo {
    /// Short code: `enospc`, `eio`, `edquot`, `write`.
    pub error: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub degraded: bool,
    #[serde(flatten)]
    pub extra: Extra,
}

/// Payload of an `m` event (`{"cl":1,"k":…}`). The nonce never appears here.
#[derive(Debug, Clone, PartialEq)]
pub enum CastMarker {
    Session(Box<SessionInfo>),
    Clock(ClockSync),
    Hello(Hello),
    Cmd(CmdRecord),
    Done(DoneRecord),
    Skip(Skip),
    Pause(PauseInfo),
    Resume(ResumeInfo),
    Nested(NestedLink),
    Multiplexer(MultiplexerInfo),
    Ctx(TmuxContext),
    Gap(GapInfo),
    Rotate(RotateInfo),
    Error(ErrorInfo),
    /// Unknown `k` (newer recorder): kept raw by readers.
    Unknown {
        k: String,
        fields: Extra,
    },
}

impl CastMarker {
    pub fn kind(&self) -> &str {
        match self {
            Self::Session(_) => "session",
            Self::Clock(_) => "clock",
            Self::Hello(_) => "hello",
            Self::Cmd(_) => "cmd",
            Self::Done(_) => "done",
            Self::Skip(_) => "skip",
            Self::Pause(_) => "pause",
            Self::Resume(_) => "resume",
            Self::Nested(_) => "nested",
            Self::Multiplexer(_) => "multiplexer",
            Self::Ctx(_) => "ctx",
            Self::Gap(_) => "gap",
            Self::Rotate(_) => "rotate",
            Self::Error(_) => "error",
            Self::Unknown { k, .. } => k,
        }
    }

    /// Parses the data string of an `m` event. `Ok(None)` means "not a cinelog marker"
    /// (no `cl` field, e.g. a third-party `m` label).
    pub fn from_json(json: &str) -> Result<Option<Self>, PayloadError> {
        let Ok(Value::Object(mut map)) = serde_json::from_str::<Value>(json) else {
            return Ok(None);
        };
        let Some(cl) = map.remove("cl") else {
            return Ok(None);
        };
        let cl = cl.as_u64().ok_or(PayloadError::MissingVersion)?;
        if cl != u64::from(PROTOCOL_VERSION) {
            return Err(PayloadError::UnsupportedVersion(cl));
        }
        let k = take_kind(&mut map)?;
        let obj = Value::Object(map);
        Ok(Some(match k.as_str() {
            "session" => Self::Session(Box::new(from_obj(obj)?)),
            "clock" => Self::Clock(from_obj(obj)?),
            "hello" => Self::Hello(from_obj(obj)?),
            "cmd" => Self::Cmd(from_obj(obj)?),
            "done" => Self::Done(from_obj(obj)?),
            "skip" => Self::Skip(from_obj(obj)?),
            "pause" => Self::Pause(from_obj(obj)?),
            "resume" => Self::Resume(from_obj(obj)?),
            "nested" => Self::Nested(from_obj(obj)?),
            "multiplexer" => Self::Multiplexer(from_obj(obj)?),
            "ctx" => Self::Ctx(from_obj(obj)?),
            "gap" => Self::Gap(from_obj(obj)?),
            "rotate" => Self::Rotate(from_obj(obj)?),
            "error" => Self::Error(from_obj(obj)?),
            _ => Self::Unknown {
                k,
                fields: into_map(obj),
            },
        }))
    }

    /// Compact JSON with `cl` first and `k` second: the data string of the `m` event.
    pub fn to_json(&self) -> String {
        let body = match self {
            Self::Session(p) => to_obj(p.as_ref()),
            Self::Clock(p) => to_obj(p),
            Self::Hello(p) => to_obj(p),
            Self::Cmd(p) => to_obj(p),
            Self::Done(p) => to_obj(p),
            Self::Skip(p) => to_obj(p),
            Self::Pause(p) => to_obj(p),
            Self::Resume(p) => to_obj(p),
            Self::Nested(p) => to_obj(p),
            Self::Multiplexer(p) => to_obj(p),
            Self::Ctx(p) => to_obj(p),
            Self::Gap(p) => to_obj(p),
            Self::Rotate(p) => to_obj(p),
            Self::Error(p) => to_obj(p),
            Self::Unknown { fields, .. } => fields.clone(),
        };
        String::from_utf8(envelope("cl", self.kind(), body)).expect("serde_json emits UTF-8")
    }

    /// Whether this marker is a command chapter (`cmd`), the only kind the server keeps
    /// as a player chapter after label rewriting (§4.5).
    pub fn is_chapter(&self) -> bool {
        matches!(self, Self::Cmd(_))
    }
}

// ------------------------------------------------------------------ CINELOG_OPTS (§9.4)

/// Shell options passed to the inner shell in `CINELOG_OPTS`, built from `[shell]`.
///
/// Format: `key=value` pairs joined by `,`; booleans are `0`/`1`. Values never contain
/// `,` or `=` (the URL is percent-encoded for those two bytes), so the plugin can parse it
/// with `_cl_opt=(${(s:,:)${CINELOG_OPTS//=/,}})`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellOpts {
    pub motd: bool,
    pub banners: bool,
    pub check_free_space: bool,
    pub external_ip: bool,
    pub title: bool,
    pub title_link: bool,
    /// Session web URL (empty before M3).
    pub url: String,
}

impl Default for ShellOpts {
    fn default() -> Self {
        Self {
            motd: true,
            banners: true,
            check_free_space: true,
            external_ip: false,
            title: true,
            title_link: false,
            url: String::new(),
        }
    }
}

impl ShellOpts {
    pub fn to_env_value(&self) -> String {
        let b = |v: bool| if v { "1" } else { "0" };
        let url = self.url.replace('%', "%25").replace(',', "%2C").replace('=', "%3D");
        format!(
            "motd={},banners={},check_free_space={},external_ip={},title={},title_link={},url={}",
            b(self.motd),
            b(self.banners),
            b(self.check_free_space),
            b(self.external_ip),
            b(self.title),
            b(self.title_link),
            url
        )
    }

    /// Lenient parse: unknown keys are ignored, missing keys keep their defaults.
    pub fn parse_env_value(s: &str) -> Self {
        let mut o = Self::default();
        for pair in s.split(',') {
            let Some((k, v)) = pair.split_once('=') else { continue };
            let flag = v == "1";
            match k {
                "motd" => o.motd = flag,
                "banners" => o.banners = flag,
                "check_free_space" => o.check_free_space = flag,
                "external_ip" => o.external_ip = flag,
                "title" => o.title = flag,
                "title_link" => o.title_link = flag,
                "url" => o.url = v.replace("%3D", "=").replace("%2C", ",").replace("%25", "%"),
                _ => {}
            }
        }
        o
    }
}

// ------------------------------------------------------------------ errors and helpers

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("nonce must be 32 lowercase hex characters")]
    Nonce,
    #[error("unknown scope {0:?}")]
    Scope(String),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PayloadError {
    #[error("payload encoding: {0}")]
    Encoding(String),
    #[error("payload JSON: {0}")]
    Json(String),
    #[error("payload has no protocol version")]
    MissingVersion,
    #[error("unsupported protocol version {0}")]
    UnsupportedVersion(u64),
    #[error("payload has no kind")]
    MissingKind,
}

impl PayloadError {
    fn json(e: serde_json::Error) -> Self {
        Self::Json(e.to_string())
    }
}

fn take_kind(map: &mut Map<String, Value>) -> Result<String, PayloadError> {
    match map.remove("k") {
        Some(Value::String(k)) => Ok(k),
        _ => Err(PayloadError::MissingKind),
    }
}

fn from_obj<T: serde::de::DeserializeOwned>(obj: Value) -> Result<T, PayloadError> {
    serde_json::from_value(obj).map_err(PayloadError::json)
}

fn to_obj<T: Serialize>(v: &T) -> Map<String, Value> {
    match serde_json::to_value(v) {
        Ok(Value::Object(m)) => m,
        _ => unreachable!("payload types serialize to JSON objects"),
    }
}

fn into_map(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

fn envelope(version_key: &str, kind: &str, body: Map<String, Value>) -> Vec<u8> {
    let mut m = Map::with_capacity(body.len() + 2);
    m.insert(version_key.to_owned(), Value::from(PROTOCOL_VERSION));
    m.insert("k".to_owned(), Value::from(kind));
    for (k, v) in body {
        if k != version_key && k != "k" {
            m.insert(k, v);
        }
    }
    serde_json::to_vec(&Value::Object(m)).expect("JSON values always serialize")
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn one() -> u32 {
    1
}

fn yes() -> bool {
    true
}

fn protocol_version() -> u32 {
    PROTOCOL_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonce_hex_roundtrip_and_rejects() {
        let n = Nonce::from_bytes([0xab; 16]);
        assert_eq!(n.to_hex(), "ab".repeat(16));
        assert_eq!(Nonce::parse_hex(n.to_hex().as_bytes()), Some(n));
        assert_eq!(Nonce::parse_hex("AB".repeat(16).as_bytes()), None);
        assert_eq!(Nonce::parse_hex(b"abc"), None);
        assert_eq!(format!("{n:?}"), "Nonce(<redacted>)");
    }

    #[test]
    fn wire_cmd_roundtrip_keeps_unknown_fields_and_order() {
        let json = br#"{"v":1,"k":"cmd","sh":42,"n":3,"typed":"printf \"a\\nb\"","expanded":"printf","cwd":"/tmp","t":1731070000.5,"future":7}"#;
        let msg = WireMessage::from_json(json).unwrap();
        let WireMessage::Cmd(c) = &msg else {
            panic!("not a cmd: {msg:?}")
        };
        assert_eq!((c.sh, c.n, c.typed.as_str()), (42, 3, "printf \"a\\nb\""));
        assert_eq!(c.extra.get("future"), Some(&Value::from(7)));
        let out = String::from_utf8(msg.to_json()).unwrap();
        assert!(out.starts_with(r#"{"v":1,"k":"cmd","sh":42"#), "{out}");
        assert_eq!(WireMessage::from_json(out.as_bytes()).unwrap(), msg);
    }

    #[test]
    fn wire_rejects_bad_envelopes() {
        assert_eq!(
            WireMessage::from_json(br#"{"k":"cmd"}"#),
            Err(PayloadError::MissingVersion)
        );
        assert_eq!(
            WireMessage::from_json(br#"{"v":2,"k":"cmd"}"#),
            Err(PayloadError::UnsupportedVersion(2))
        );
        assert_eq!(WireMessage::from_json(br#"{"v":1}"#), Err(PayloadError::MissingKind));
        assert!(matches!(
            WireMessage::from_json(br#"{"v":1,"k":"cmd","sh":1}"#),
            Err(PayloadError::Json(_))
        ));
        let unknown = WireMessage::from_json(br#"{"v":1,"k":"telemetry","x":1}"#).unwrap();
        assert_eq!(unknown.kind(), "telemetry");
    }

    #[test]
    fn nested_requires_recorder_nonce() {
        let json = br#"{"v":1,"k":"nested-start","uuid":"u","pid":5,"starttime":9,"scope":"team"}"#;
        let msg = WireMessage::from_json(json).unwrap();
        assert!(msg.requires_recorder_nonce());
        assert!(
            !WireMessage::Skip(Skip {
                sh: 1,
                t: 0.0,
                extra: Extra::new()
            })
            .requires_recorder_nonce()
        );
    }

    #[test]
    fn cast_marker_cmd_has_cl_first_and_seq() {
        let cmd = Cmd {
            sh: 7,
            n: 1,
            typed: "ls".into(),
            expanded: "ls --color=tty".into(),
            cwd: "/".into(),
            user: "u".into(),
            euid: Some(1000),
            host: "h".into(),
            pane: None,
            t: 1.0,
            trunc: false,
            extra: Extra::new(),
        };
        let m = CastMarker::Cmd(CmdRecord { seq: 4, cmd });
        let s = m.to_json();
        assert!(s.starts_with(r#"{"cl":1,"k":"cmd","seq":4,"sh":7"#), "{s}");
        assert!(!s.contains("trunc"));
        assert_eq!(CastMarker::from_json(&s).unwrap(), Some(m));
    }

    #[test]
    fn cast_marker_foreign_labels_are_not_markers() {
        assert_eq!(CastMarker::from_json("chapter 1").unwrap(), None);
        assert_eq!(CastMarker::from_json(r#"{"label":"x"}"#).unwrap(), None);
        assert!(CastMarker::from_json(r#"{"cl":9,"k":"cmd"}"#).is_err());
    }

    #[test]
    fn session_marker_defaults() {
        let s = r#"{"cl":1,"k":"session","uuid":"0190","start_epoch_us":5,"scope":"personal","user":"u","attribution":"none"}"#;
        let Some(CastMarker::Session(info)) = CastMarker::from_json(s).unwrap() else {
            panic!()
        };
        assert_eq!((info.part, info.protocol, info.input), (1, 1, true));
    }

    #[test]
    fn shell_opts_roundtrip() {
        let o = ShellOpts {
            motd: false,
            url: "https://h:10000/s/x?cmd=1,2".into(),
            ..ShellOpts::default()
        };
        let env = o.to_env_value();
        assert!(!env.split(',').any(|p| p.matches('=').count() != 1), "{env}");
        assert_eq!(ShellOpts::parse_env_value(&env), o);
        assert_eq!(
            ShellOpts::parse_env_value("garbage,motd=0"),
            ShellOpts {
                motd: false,
                ..ShellOpts::default()
            }
        );
    }

    #[test]
    fn encoding_threshold() {
        assert_eq!(Encoding::for_json_len(BASE64_MAX_JSON), Encoding::Base64);
        assert_eq!(Encoding::for_json_len(BASE64_MAX_JSON + 1), Encoding::Json);
        assert_eq!(Encoding::from_byte(b'j'), Some(Encoding::Json));
    }
}
