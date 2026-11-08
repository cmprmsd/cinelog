//! Wire-format markers written from docs/PROTOCOL.md §1, used to forge markers that a
//! recorder must reject (I7).

/// `ESC ] 7777 ; cinelog ; 1 ; <nonce> ; b ; <base64 json> BEL`.
pub fn marker_b64(nonce: &str, json: &str) -> String {
    format!(
        "\x1b]7777;cinelog;1;{nonce};b;{}\x07",
        crate::util::base64_nopad(json.as_bytes())
    )
}

/// The `enc=j` form (control bytes would have to be escaped; callers pass plain JSON).
pub fn marker_json(nonce: &str, json: &str) -> String {
    format!("\x1b]7777;cinelog;1;{nonce};j;{json}\x1b\\")
}

/// A forged `cmd` payload.
pub fn cmd_json(sh: u32, n: u64, typed: &str) -> String {
    serde_json::json!({"v": 1, "k": "cmd", "sh": sh, "n": n, "typed": typed}).to_string()
}

/// A forged `nested-start` that no process tree corroborates.
pub fn nested_start_json(uuid: &str) -> String {
    serde_json::json!({
        "v": 1, "k": "nested-start", "uuid": uuid, "pid": 1, "starttime": 1, "scope": "team",
        "user": "root", "sudo_user": "mallory"
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_shapes() {
        let m = marker_b64(&"0".repeat(32), "{}");
        assert_eq!(m, format!("\x1b]7777;cinelog;1;{};b;e30\x07", "0".repeat(32)));
        assert!(marker_json("-", "{\"v\":1}").ends_with("\x1b\\"));
        let j: serde_json::Value = serde_json::from_str(&cmd_json(7, 1, "x")).unwrap();
        assert_eq!(j["k"], "cmd");
        let j: serde_json::Value = serde_json::from_str(&nested_start_json("u")).unwrap();
        assert_eq!(j["k"], "nested-start");
    }
}
