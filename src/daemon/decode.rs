//! Compressed request bodies on typed ingresses. The TypeSafe SDKs send
//! bodies of 1 KiB or more with `Content-Encoding: zstd` by default, so a
//! typed ingress (which reads the body) decodes `zstd` and `gzip` before
//! anything else and drops the header. `/v1/raw` sends bytes unchanged.

use std::io::Read;

use axum::http::{HeaderMap, header};
use bytes::Bytes;

use crate::error::S1Error;

/// Decode the body per `Content-Encoding` and remove the header. One layer
/// only; `identity` is accepted. All gzip members and zstd frames are
/// decoded, and the joined decoded body is bounded by `max`.
pub fn decode_body(headers: &mut HeaderMap, body: Bytes, max: usize) -> Result<Bytes, S1Error> {
    let Some(enc) = headers.get(header::CONTENT_ENCODING) else {
        return Ok(body);
    };
    let enc = enc
        .to_str()
        .map_err(|_| S1Error::InvalidRequest("Content-Encoding is not ASCII".into()))?
        .trim()
        .to_ascii_lowercase();
    let decoded = match enc.as_str() {
        "" | "identity" => body,
        "zstd" => {
            let r = zstd::stream::read::Decoder::new(&body[..])
                .map_err(|e| S1Error::InvalidRequest(format!("bad zstd body: {e}")))?;
            read_bounded(r, max, "zstd")?
        }
        "gzip" | "x-gzip" => {
            // A gzip body may hold several members; decode all of them.
            read_bounded(flate2::read::MultiGzDecoder::new(&body[..]), max, "gzip")?
        }
        other => {
            return Err(S1Error::InvalidRequest(format!(
                "Content-Encoding `{other}` is not supported; use zstd, gzip, or none"
            )));
        }
    };
    headers.remove(header::CONTENT_ENCODING);
    headers.remove(header::CONTENT_LENGTH);
    Ok(decoded)
}

fn read_bounded(r: impl Read, max: usize, what: &str) -> Result<Bytes, S1Error> {
    let mut out = Vec::new();
    r.take(max as u64 + 1)
        .read_to_end(&mut out)
        .map_err(|e| S1Error::InvalidRequest(format!("bad {what} body: {e}")))?;
    if out.len() > max {
        return Err(S1Error::TooLong(format!(
            "decoded body is over {max} bytes"
        )));
    }
    Ok(Bytes::from(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use std::io::Write;

    fn with(enc: &'static str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::CONTENT_ENCODING, HeaderValue::from_static(enc));
        h
    }

    #[test]
    fn decodes_zstd_and_gzip() {
        let plain = br#"{"model":"decide"}"#.repeat(100);
        let z = zstd::encode_all(&plain[..], 3).unwrap();
        let mut h = with("zstd");
        assert_eq!(decode_body(&mut h, Bytes::from(z), 1 << 20).unwrap(), plain);
        assert!(h.get(header::CONTENT_ENCODING).is_none());

        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&plain).unwrap();
        let mut h = with("gzip");
        let out = decode_body(&mut h, Bytes::from(gz.finish().unwrap()), 1 << 20).unwrap();
        assert_eq!(out, plain);

        let mut none = HeaderMap::new();
        assert_eq!(
            decode_body(&mut none, Bytes::from_static(b"x"), 10).unwrap(),
            "x"
        );
    }

    fn gz(plain: &[u8]) -> Vec<u8> {
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(plain).unwrap();
        gz.finish().unwrap()
    }

    #[test]
    fn decodes_every_member_or_frame() {
        // A gzip body may hold several members and a zstd body several
        // frames; the decoded body is all of them joined.
        let head = br#"{"model":"jev-latest","state":""#;
        let tail = format!("{}\"}}", "a".repeat(60_000));
        let mut plain = head.to_vec();
        plain.extend_from_slice(tail.as_bytes());

        let mut body = gz(head);
        body.extend(gz(tail.as_bytes()));
        let mut h = with("gzip");
        assert_eq!(
            decode_body(&mut h, Bytes::from(body), 1 << 20).unwrap(),
            plain
        );

        let mut body = zstd::encode_all(&head[..], 3).unwrap();
        body.extend(zstd::encode_all(tail.as_bytes(), 3).unwrap());
        let mut h = with("zstd");
        assert_eq!(
            decode_body(&mut h, Bytes::from(body), 1 << 20).unwrap(),
            plain
        );
    }

    #[test]
    fn size_cap_counts_all_members() {
        // Each member is under the cap; together they are over it.
        let mut body = gz(&[b'a'; 8]);
        body.extend(gz(&[b'a'; 8]));
        let e = decode_body(&mut with("gzip"), Bytes::from(body), 10).unwrap_err();
        assert_eq!(e.code(), "too_long");
    }

    #[test]
    fn rejects_unknown_bad_and_oversized() {
        let e = decode_body(&mut with("br"), Bytes::from_static(b"x"), 10).unwrap_err();
        assert_eq!(e.code(), "invalid_request");
        let e = decode_body(&mut with("zstd"), Bytes::from_static(b"nope"), 10).unwrap_err();
        assert_eq!(e.code(), "invalid_request");
        let big = zstd::encode_all(&[b'a'; 100][..], 3).unwrap();
        let e = decode_body(&mut with("zstd"), Bytes::from(big), 10).unwrap_err();
        assert_eq!(e.code(), "too_long");
    }
}
