//! Image file paths in systemone `assets` (spec "Large inputs").
//!
//! A local client may name an image file instead of sending its bytes:
//!
//! ```json
//! "assets": {"photo-1": {"type": "image", "s1_path": "/abs/path/photo.png"}}
//! ```
//!
//! `s1_path` is not a TypeSafe asset field (those are `type`, `format`,
//! `data_base64`, `part`), so a path-valued asset cannot be mistaken for real
//! provider input. The daemon replaces each such asset with
//! `{"type": "image", "format": <detected>, "data_base64": <file bytes>}`
//! before it routes the request, so everything after that point (routing,
//! and later the cache key) sees the image content, never the path.
//!
//! Paths are accepted only on the Unix socket ([`Origin::UnixSocket`]). On
//! TCP the request is rejected and the file is never read.

use std::path::Path;

use base64::Engine;
use bytes::Bytes;
use std::fmt;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;

use serde::de::{Deserialize, Deserializer, MapAccess, Visitor};
use serde_json::Value;
use serde_json::value::RawValue;

use crate::error::S1Error;

/// Asset field that holds a local file path.
pub const PATH_FIELD: &str = "s1_path";

/// Largest image file s1 reads (TypeSafe's per-image limit, decoded).
pub const MAX_IMAGE_FILE: u64 = 16 * 1024 * 1024;

/// Which listener a request came in on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// The Unix socket: local, protected by file permissions.
    UnixSocket,
    /// TCP (or unknown): never reads local files.
    Tcp,
}

/// PNG or JPEG, from the magic bytes.
pub fn image_format(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("jpg")
    } else {
        None
    }
}

/// A path-valued asset: `{"type": "image", "s1_path": path}`.
pub fn path_asset(path: &Path) -> Value {
    serde_json::json!({"type": "image", PATH_FIELD: path.to_string_lossy()})
}

/// Replace path-valued assets in a systemone body with base64 assets.
///
/// Returns `None` when the body has no path-valued asset (the original bytes
/// are then sent unchanged). `max_body` bounds the expanded body.
///
/// Only the path-valued assets are rebuilt. Every other member (top level
/// and other assets) keeps its exact bytes, so for example a number that
/// does not fit a float or `u64` is not changed.
pub fn expand_paths(
    body: &[u8],
    origin: Origin,
    max_body: usize,
) -> Result<Option<Bytes>, S1Error> {
    // Cheap check first: most bodies never mention the field. A `\u`
    // escape can spell the key without its literal bytes, so bodies with
    // one are parsed too.
    if !contains(body, PATH_FIELD.as_bytes()) && !contains(body, b"\\u") {
        return Ok(None);
    }
    let Ok(Members(root)) = serde_json::from_slice::<Members<'_>>(body) else {
        return Ok(None);
    };
    // Each `assets` member: its members, and the path-valued ones (decoded).
    let mut groups: Vec<(usize, Vec<(String, &RawValue)>)> = Vec::new();
    let mut found: Vec<(usize, usize, Option<String>)> = Vec::new();
    for (i, (key, raw)) in root.iter().enumerate() {
        if key != "assets" {
            continue;
        }
        let Ok(Members(assets)) = serde_json::from_str::<Members<'_>>(raw.get()) else {
            continue;
        };
        for (j, (_, a)) in assets.iter().enumerate() {
            let Ok(Members(fields)) = serde_json::from_str::<Members<'_>>(a.get()) else {
                continue;
            };
            if let Some((_, p)) = fields.iter().find(|(k, _)| k == PATH_FIELD) {
                let path = serde_json::from_str::<String>(p.get()).ok();
                found.push((groups.len(), j, path));
            }
        }
        groups.push((i, assets));
    }
    if found.is_empty() {
        return Ok(None);
    }
    if origin != Origin::UnixSocket {
        return Err(S1Error::InvalidRequest(format!(
            "image file paths (`assets.*.{PATH_FIELD}`) are accepted only on the Unix socket; \
             over TCP send `data_base64`"
        )));
    }
    let mut added = 0usize;
    let mut rebuilt: Vec<(usize, usize, String)> = Vec::new();
    for (g, j, path) in found {
        let (name, raw) = &groups[g].1[j];
        let path = match path {
            Some(p) => p,
            None => {
                return Err(S1Error::InvalidRequest(format!(
                    "asset `{name}`: `{PATH_FIELD}` must be a string"
                )));
            }
        };
        let data = read_image(name, Path::new(&path))?;
        added += data.len().div_ceil(3) * 4;
        if body.len() + added > max_body {
            return Err(too_long(max_body));
        }
        let format = image_format(&data).expect("checked in read_image");
        rebuilt.push((g, j, image_asset(raw, format, &data)));
    }
    let group_text: Vec<String> = groups
        .iter()
        .enumerate()
        .map(|(g, (_, assets))| {
            write_object(assets.iter().enumerate().map(|(j, (k, raw))| {
                let text = rebuilt
                    .iter()
                    .find(|(rg, rj, _)| *rg == g && *rj == j)
                    .map_or(raw.get(), |(_, _, t)| t.as_str());
                (k.as_str(), text)
            }))
        })
        .collect();
    let out = write_object(root.iter().enumerate().map(|(i, (k, raw))| {
        let text = groups
            .iter()
            .position(|(gi, _)| *gi == i)
            .map_or(raw.get(), |g| group_text[g].as_str());
        (k.as_str(), text)
    }));
    // The running check above counts only the base64 data; the rebuilt
    // members (`type`, `format`, `data_base64`) add a few bytes more.
    if out.len() > max_body {
        return Err(too_long(max_body));
    }
    Ok(Some(Bytes::from(out)))
}

fn too_long(max_body: usize) -> S1Error {
    S1Error::TooLong(format!(
        "request with images read from files is over {} MiB",
        max_body / (1024 * 1024)
    ))
}

/// The members of a JSON object in order, keys decoded, values as raw
/// bytes (duplicates kept).
struct Members<'a>(Vec<(String, &'a RawValue)>);

impl<'de> Deserialize<'de> for Members<'de> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Members<'de>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a JSON object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<Members<'de>, A::Error> {
                let mut out = Vec::new();
                while let Some((k, v)) = m.next_entry::<String, &'de RawValue>()? {
                    out.push((k, v));
                }
                Ok(Members(out))
            }
        }
        d.deserialize_map(V)
    }
}

/// `{"k":<raw>,...}` from keys and raw JSON values.
fn write_object<'a>(members: impl Iterator<Item = (&'a str, &'a str)>) -> String {
    let mut out = String::from("{");
    for (n, (k, v)) in members.enumerate() {
        if n > 0 {
            out.push(',');
        }
        out.push_str(&serde_json::to_string(k).expect("string serializes"));
        out.push(':');
        out.push_str(v);
    }
    out.push('}');
    out
}

/// The asset with the path replaced by inline data. Other members keep
/// their bytes.
fn image_asset(old: &RawValue, format: &str, data: &[u8]) -> String {
    let Members(fields) = serde_json::from_str(old.get()).expect("checked as an object");
    let format = Value::String(format.into()).to_string();
    let image = Value::String("image".into()).to_string();
    let b64 = Value::String(base64::engine::general_purpose::STANDARD.encode(data)).to_string();
    let kept = fields
        .iter()
        .filter(|(k, _)| {
            !matches!(
                k.as_str(),
                PATH_FIELD | "type" | "format" | "data_base64" | "part"
            )
        })
        .map(|(k, v)| (k.as_str(), v.get()));
    write_object(
        [("type", image.as_str())]
            .into_iter()
            .chain(kept)
            .chain([("format", format.as_str()), ("data_base64", b64.as_str())]),
    )
}

fn read_image(name: &str, path: &Path) -> Result<Vec<u8>, S1Error> {
    let shown = path.display();
    if !path.is_absolute() {
        return Err(S1Error::InvalidRequest(format!(
            "asset `{name}`: image path {shown} must be absolute"
        )));
    }
    let cannot = |e: std::io::Error| {
        S1Error::InvalidRequest(format!("asset `{name}`: cannot read image {shown}: {e}"))
    };
    // Open once and check that handle. O_NONBLOCK: opening a FIFO must not
    // wait for a writer (it has no effect on regular files).
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .map_err(cannot)?;
    let meta = file.metadata().map_err(cannot)?;
    if !meta.is_file() {
        return Err(S1Error::InvalidRequest(format!(
            "asset `{name}`: image {shown} is not a file"
        )));
    }
    let too_long = |len: u64| {
        S1Error::TooLong(format!(
            "asset `{name}`: image {shown} is {len} bytes or more; the limit is {MAX_IMAGE_FILE} (16 MiB)"
        ))
    };
    if meta.len() > MAX_IMAGE_FILE {
        return Err(too_long(meta.len()));
    }
    // The file can grow after the check: read at most one byte too many.
    let data = read_at_most(file, MAX_IMAGE_FILE)
        .map_err(cannot)?
        .ok_or_else(|| too_long(MAX_IMAGE_FILE + 1))?;
    if image_format(&data).is_none() {
        return Err(S1Error::InvalidRequest(format!(
            "asset `{name}`: image {shown} is not PNG or JPEG"
        )));
    }
    Ok(data)
}

/// All bytes of `r` if there are at most `max`; `None` if there are more.
/// Never reads more than `max + 1` bytes.
fn read_at_most(r: impl Read, max: u64) -> std::io::Result<Option<Vec<u8>>> {
    let mut data = Vec::new();
    r.take(max + 1).read_to_end(&mut data)?;
    Ok((data.len() as u64 <= max).then_some(data))
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nrest";

    fn body_with(path: &Path) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "model": "jev-latest",
            "state": "img-1",
            "questions": {"q": {"type": "noul"}},
            "assets": {"img-1": path_asset(path)}
        }))
        .unwrap()
    }

    #[test]
    fn bodies_without_paths_are_left_alone() {
        let body = br#"{"model":"m","state":"x","assets":{"a":{"type":"image","format":"png","data_base64":"AA=="}}}"#;
        assert_eq!(expand_paths(body, Origin::Tcp, 1 << 20).unwrap(), None);
        // Text that merely mentions the field is not an asset.
        let body = br#"{"model":"m","state":"s1_path"}"#;
        assert_eq!(expand_paths(body, Origin::Tcp, 1 << 20).unwrap(), None);
    }

    #[test]
    fn socket_reads_and_encodes_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.png");
        std::fs::write(&p, PNG).unwrap();
        let out = expand_paths(&body_with(&p), Origin::UnixSocket, 1 << 20)
            .unwrap()
            .unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(
            v["assets"]["img-1"],
            json!({
                "type": "image",
                "format": "png",
                "data_base64": base64::engine::general_purpose::STANDARD.encode(PNG)
            })
        );
        assert_eq!(v["state"], "img-1");
        assert!(!String::from_utf8_lossy(&out).contains(PATH_FIELD));
    }

    #[test]
    fn tcp_rejects_paths_without_reading() {
        // The file does not exist: a read attempt would give another error.
        let body = body_with(Path::new("/nonexistent/s1/never.png"));
        let e = expand_paths(&body, Origin::Tcp, 1 << 20).unwrap_err();
        assert_eq!(e.code(), "invalid_request");
        assert!(e.to_string().contains("Unix socket"), "{e}");
    }

    #[test]
    fn missing_relative_and_non_image_files_fail_clearly() {
        let e = expand_paths(
            &body_with(Path::new("/nonexistent/s1/never.png")),
            Origin::UnixSocket,
            1 << 20,
        )
        .unwrap_err();
        assert_eq!(e.code(), "invalid_request");
        assert!(e.to_string().contains("cannot read image"), "{e}");

        let e = expand_paths(
            &body_with(Path::new("rel.png")),
            Origin::UnixSocket,
            1 << 20,
        )
        .unwrap_err();
        assert!(e.to_string().contains("absolute"), "{e}");

        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.txt");
        std::fs::write(&p, b"hello").unwrap();
        let e = expand_paths(&body_with(&p), Origin::UnixSocket, 1 << 20).unwrap_err();
        assert!(e.to_string().contains("not PNG or JPEG"), "{e}");
    }

    #[test]
    fn oversized_files_and_bodies_are_too_long() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("big.png");
        let f = std::fs::File::create(&p).unwrap();
        f.set_len(MAX_IMAGE_FILE + 1).unwrap();
        let e = expand_paths(&body_with(&p), Origin::UnixSocket, usize::MAX).unwrap_err();
        assert_eq!(e.code(), "too_long");
        assert!(e.to_string().contains("16 MiB"), "{e}");

        // Small file, but the expanded body would pass the body limit.
        let small = dir.path().join("small.png");
        std::fs::write(&small, PNG).unwrap();
        let body = body_with(&small);
        let e = expand_paths(&body, Origin::UnixSocket, body.len() + 4).unwrap_err();
        assert_eq!(e.code(), "too_long");
    }

    #[test]
    fn replacement_field_overhead_counts_toward_the_body_limit() {
        // An asset without `type` gains `"type":"image"`, `"format"` and
        // `"data_base64"` members: the expanded body is longer than the
        // original body plus the base64 data alone.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.png");
        std::fs::write(&p, PNG).unwrap();
        let body = serde_json::to_vec(&json!({
            "model": "jev-latest",
            "state": "img-1",
            "questions": {"q": {"type": "noul"}},
            "assets": {"img-1": {PATH_FIELD: p.to_string_lossy()}}
        }))
        .unwrap();
        let full = expand_paths(&body, Origin::UnixSocket, usize::MAX)
            .unwrap()
            .unwrap();
        let data_len = PNG.len().div_ceil(3) * 4;
        assert!(
            body.len() + data_len < full.len(),
            "test needs field overhead: {} + {data_len} vs {}",
            body.len(),
            full.len()
        );
        let e = expand_paths(&body, Origin::UnixSocket, full.len() - 1).unwrap_err();
        assert_eq!(e.code(), "too_long");
        assert!(
            expand_paths(&body, Origin::UnixSocket, full.len())
                .unwrap()
                .is_some()
        );
    }

    fn escaped_body(path: &Path) -> Vec<u8> {
        // `s1_path` with its underscore written as a JSON escape.
        format!(
            r#"{{"model":"jev-latest","state":"img-1","questions":{{"q":{{"type":"noul"}}}},"assets":{{"img-1":{{"type":"image","{key}":{path}}}}}}}"#,
            key = format_args!("s1{}u005fpath", char::from(0x5c_u8)),
            path = serde_json::to_string(&path.to_string_lossy()).unwrap()
        )
        .into_bytes()
    }

    #[test]
    fn reads_stop_one_byte_past_the_limit() {
        // An endless source (a file that keeps growing) is not read to the end.
        assert_eq!(read_at_most(std::io::repeat(7), 10).unwrap(), None);
        assert_eq!(
            read_at_most(&[1u8, 2, 3][..], 3).unwrap(),
            Some(vec![1, 2, 3])
        );
    }

    #[test]
    fn fifos_are_refused_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("pipe.png");
        let c = std::ffi::CString::new(p.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let e = read_image("a", &p).unwrap_err();
        assert!(e.to_string().contains("not a file"), "{e}");
    }

    #[test]
    fn escaped_path_keys_are_found_on_both_transports() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.png");
        std::fs::write(&p, PNG).unwrap();
        let body = escaped_body(&p);
        assert!(
            !String::from_utf8_lossy(&body).contains(PATH_FIELD),
            "{}",
            String::from_utf8_lossy(&body)
        );
        let out = expand_paths(&body, Origin::UnixSocket, 1 << 20)
            .unwrap()
            .expect("escaped key is a path asset");
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["assets"]["img-1"]["format"], "png");
        let e = expand_paths(&body, Origin::Tcp, 1 << 20).unwrap_err();
        assert_eq!(e.code(), "invalid_request");
    }

    #[test]
    fn other_members_keep_their_exact_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.png");
        std::fs::write(&p, PNG).unwrap();
        let path = serde_json::to_string(&p.to_string_lossy()).unwrap();
        let body = format!(
            r#"{{"model":"jev-latest","state":{{"n":18446744073709551617,"f":1.10}},"questions":{{"q":{{"type":"noul"}}}},"assets":{{"keep":{{"type":"image","format":"png","data_base64":"AA==","x":1e400}},"img-1":{{"type":"image","s1_path":{path}}}}}}}"#
        );
        let out = expand_paths(body.as_bytes(), Origin::UnixSocket, 1 << 20)
            .unwrap()
            .unwrap();
        let out = String::from_utf8(out.to_vec()).unwrap();
        assert!(
            out.contains(r#""state":{"n":18446744073709551617,"f":1.10}"#),
            "{out}"
        );
        assert!(
            out.contains(
                r#""keep":{"type":"image","format":"png","data_base64":"AA==","x":1e400}"#
            ),
            "{out}"
        );
        assert!(!out.contains(PATH_FIELD), "{out}");
    }
}
