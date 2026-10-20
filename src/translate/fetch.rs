//! Image URLs in an OpenAI Decisions `input`, fetched by s1 and inlined as
//! data URLs. Only toward systemone (spec: "URLs are fetched by s1 only when
//! going to systemone"); OpenAI fetches public URLs itself.
//!
//! The spec allows "data URLs or public HTTPS URLs", so by default s1
//! fetches only `https` URLs whose host resolves to public addresses only.
//! The check runs in the DNS resolver of the HTTP client, so the address
//! that was checked is the address that is connected to (no DNS rebinding),
//! and again for every redirect. No proxy is used.
//!
//! Each image must be PNG or JPEG (from its bytes) and at most
//! [`MAX_IMAGE_BYTES`]. All images of one request share a budget: the
//! expanded body must stay within the body limit, checked while the bytes
//! arrive. At most [`MAX_CONCURRENT_FETCHES`] downloads run at a time in the
//! whole daemon. A failed fetch is `invalid_request`. Errors and logs name
//! an image by its position (`input[0].content[1]`), never by its URL,
//! because URLs can carry signed query strings.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use base64::Engine;
use bytes::Bytes;
use reqwest::Url;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use serde_json::Value;
use tokio::sync::Semaphore;

use crate::error::S1Error;
use crate::images::image_format;

/// Largest image s1 fetches (TypeSafe's per-image limit, decoded).
pub const MAX_IMAGE_BYTES: usize = crate::images::MAX_IMAGE_FILE as usize;
/// Most images in one request (OpenAI Decisions limit).
pub const MAX_IMAGES: usize = 128;
/// Most image downloads in flight at once, over all requests.
pub const MAX_CONCURRENT_FETCHES: usize = 16;
const MAX_REDIRECTS: usize = 5;

static FETCH_SLOTS: Semaphore = Semaphore::const_new(MAX_CONCURRENT_FETCHES);

/// Which image URLs s1 may fetch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FetchPolicy {
    /// Also fetch `http` URLs and non-public addresses (loopback, private,
    /// link-local). Off by default; tests that serve images on loopback
    /// turn it on.
    pub allow_local: bool,
}

impl FetchPolicy {
    /// The spec's policy: public HTTPS URLs only.
    pub const PUBLIC_HTTPS: FetchPolicy = FetchPolicy { allow_local: false };
    /// Any HTTP(S) URL, for tests with local image servers.
    pub const ALLOW_LOCAL: FetchPolicy = FetchPolicy { allow_local: true };
}

/// Whether s1 may connect to `ip` under the public-only policy: not
/// loopback, private, link-local, shared, documentation, multicast,
/// reserved, or an IPv6 form that embeds such an IPv4 address.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => is_public_v6(v6),
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_multicast()
        || a == 0
        || (a == 100 && (64..128).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 198 && (18..20).contains(&b))
        || a >= 240)
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_public_v4(v4);
    }
    let s = ip.segments();
    // Global unicast is 2000::/3; everything else (loopback, unique local,
    // link-local, multicast, NAT64, IPv4-compatible) is refused.
    if s[0] & 0xe000 != 0x2000 {
        return false;
    }
    // 2001::/23 (IETF protocol assignments, Teredo) and 2001:db8::/32.
    if s[0] == 0x2001 && (s[1] < 0x0200 || s[1] == 0x0db8) {
        return false;
    }
    // 6to4 embeds an IPv4 address.
    if s[0] == 0x2002 {
        let v4 = Ipv4Addr::new((s[1] >> 8) as u8, s[1] as u8, (s[2] >> 8) as u8, s[2] as u8);
        return is_public_v4(v4);
    }
    true
}

/// Whether `url` may be fetched (or followed as a redirect) under
/// `policy`. A host name is checked again when it is resolved.
fn check_target(url: &Url, policy: FetchPolicy) -> Result<(), String> {
    match url.scheme() {
        "https" => {}
        "http" if policy.allow_local => {}
        other => {
            return Err(format!(
                "only public HTTPS image URLs are fetched, not `{other}`"
            ));
        }
    }
    if policy.allow_local {
        return Ok(());
    }
    let host = url
        .host_str()
        .ok_or_else(|| "the image URL has no host".to_string())?;
    let literal = host.trim_start_matches('[').trim_end_matches(']');
    let Ok(ip) = literal.parse::<IpAddr>() else {
        return Ok(()); // a name: checked when it is resolved
    };
    if is_public(ip) {
        Ok(())
    } else {
        Err("the image URL's host is not a public address".into())
    }
}

/// A DNS resolver that refuses names with any non-public address.
struct PublicResolver;

impl Resolve for PublicResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> =
                tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            if addrs.is_empty() {
                return Err("the image host has no address".into());
            }
            if addrs.iter().any(|a| !is_public(a.ip())) {
                return Err("the image host resolves to a non-public address".into());
            }
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

/// Whether the public-only client follows a redirect to `url` after
/// `hops` earlier URLs: at most [`MAX_REDIRECTS`], each target checked like
/// the first URL ([`check_target`]).
fn public_redirect(url: &Url, hops: usize) -> Result<(), String> {
    if hops > MAX_REDIRECTS {
        return Err("too many redirects".into());
    }
    check_target(url, FetchPolicy::PUBLIC_HTTPS).map_err(|why| format!("redirect refused: {why}"))
}

fn base_client() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .no_proxy()
}

/// The client for [`FetchPolicy::PUBLIC_HTTPS`]: public-only DNS and
/// [`public_redirect`] on every redirect.
fn public_client() -> reqwest::Client {
    base_client()
        .dns_resolver(PublicResolver)
        .redirect(reqwest::redirect::Policy::custom(
            |attempt| match public_redirect(attempt.url(), attempt.previous().len()) {
                Ok(()) => attempt.follow(),
                Err(why) => attempt.error(why),
            },
        ))
        .build()
        .expect("build image HTTP client")
}

fn client(policy: FetchPolicy) -> &'static reqwest::Client {
    static PUBLIC: OnceLock<reqwest::Client> = OnceLock::new();
    static LOCAL: OnceLock<reqwest::Client> = OnceLock::new();
    if policy.allow_local {
        LOCAL.get_or_init(|| {
            base_client()
                .redirect(reqwest::redirect::Policy::limited(MAX_REDIRECTS))
                .build()
                .expect("build image HTTP client")
        })
    } else {
        PUBLIC.get_or_init(public_client)
    }
}

fn is_url(s: &str) -> bool {
    let lower = s.get(..8).unwrap_or(s).to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

/// HTTP(S) image URLs in an OpenAI `input`, in order, with their position
/// (`input[i].content[j]`).
pub fn image_urls(body: &Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (i, m) in body
        .get("input")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        for (j, p) in m
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
        {
            if p.get("type").and_then(Value::as_str) == Some("input_image")
                && let Some(url) = p.get("image_url").and_then(Value::as_str)
                && is_url(url)
            {
                out.push((format!("input[{i}].content[{j}]"), url.to_string()));
            }
        }
    }
    out
}

/// Cheap byte check before a parse: the body can hold an image URL only
/// if it has the literal key `image_url`, or a JSON escape (a key can be
/// spelled `"image\u005furl"`).
fn may_have_image_urls(body: &[u8]) -> bool {
    body.windows(9).any(|w| w == b"image_url") || body.windows(2).any(|w| w == b"\\u")
}

/// Whether an OpenAI Decisions request has an HTTP(S) image URL.
pub fn has_image_urls(body: &[u8]) -> bool {
    may_have_image_urls(body)
        && serde_json::from_slice::<Value>(body).is_ok_and(|v| !image_urls(&v).is_empty())
}

/// An error and its sources, without any URL.
fn describe(e: reqwest::Error) -> String {
    let e = e.without_url();
    let mut out = e.to_string();
    let mut src = std::error::Error::source(&e);
    while let Some(s) = src {
        out.push_str(": ");
        out.push_str(&s.to_string());
        src = s.source();
    }
    out
}

/// Take `n` bytes from the request's shared budget.
fn spend(budget: &AtomicUsize, n: usize) -> bool {
    budget
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |left| {
            left.checked_sub(n)
        })
        .is_ok()
}

/// Fetch one image (named `at` in errors) and return it as a data URL.
/// Its bytes come out of `budget` as they arrive.
async fn fetch_data_url(
    at: &str,
    url: &str,
    policy: FetchPolicy,
    budget: &AtomicUsize,
) -> Result<String, S1Error> {
    let fail = |why: String| S1Error::InvalidRequest(format!("cannot fetch image {at}: {why}"));
    let over = || {
        S1Error::TooLong(format!(
            "image {at} is over {MAX_IMAGE_BYTES} bytes, or the images make the request too large"
        ))
    };
    let parsed = Url::parse(url).map_err(|_| fail("not a valid URL".into()))?;
    check_target(&parsed, policy).map_err(fail)?;
    let _slot = FETCH_SLOTS
        .acquire()
        .await
        .map_err(|_| fail("the image fetcher is closed".into()))?;
    let mut resp = client(policy)
        .get(parsed)
        .send()
        .await
        .map_err(|e| fail(describe(e)))?;
    if !resp.status().is_success() {
        return Err(fail(format!("HTTP {}", resp.status().as_u16())));
    }
    if resp
        .content_length()
        .is_some_and(|n| n > MAX_IMAGE_BYTES as u64)
    {
        return Err(over());
    }
    let mut data: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| fail(describe(e)))? {
        if data.len() + chunk.len() > MAX_IMAGE_BYTES || !spend(budget, chunk.len()) {
            return Err(over());
        }
        data.extend_from_slice(&chunk);
    }
    let mime = match image_format(&data) {
        Some("png") => "image/png",
        Some(_) => "image/jpeg",
        None => {
            return Err(S1Error::Unsupported(format!(
                "image {at} is not PNG or JPEG; systemone takes only those"
            )));
        }
    };
    let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
    Ok(format!("data:{mime};base64,{b64}"))
}

/// Replace HTTP(S) image URLs in an OpenAI Decisions request with data
/// URLs. `None` when the body has no image URL (send it unchanged). The
/// result is at most `max_body` bytes, else `too_long`.
pub async fn inline_image_urls(
    body: &[u8],
    policy: FetchPolicy,
    max_body: usize,
) -> Result<Option<Bytes>, S1Error> {
    // Cheap check first: most bodies have no image URL.
    if !may_have_image_urls(body) {
        return Ok(None);
    }
    let Ok(mut v) = serde_json::from_slice::<Value>(body) else {
        return Ok(None);
    };
    let urls = image_urls(&v);
    if urls.is_empty() {
        return Ok(None);
    }
    if urls.len() > MAX_IMAGES {
        return Err(S1Error::InvalidRequest(format!(
            "{} image URLs; at most {MAX_IMAGES} images per request",
            urls.len()
        )));
    }
    let too_long = || {
        S1Error::TooLong(format!(
            "the request with its images inlined is over {max_body} bytes"
        ))
    };
    // Decoded bytes that fit: base64 makes 3 bytes into 4.
    let budget = Arc::new(AtomicUsize::new(
        max_body.saturating_sub(body.len()) / 4 * 3,
    ));
    let mut set = tokio::task::JoinSet::new();
    for (i, (at, url)) in urls.iter().cloned().enumerate() {
        let budget = budget.clone();
        set.spawn(async move { (i, fetch_data_url(&at, &url, policy, &budget).await) });
    }
    let mut fetched: Vec<Option<String>> = vec![None; urls.len()];
    while let Some(joined) = set.join_next().await {
        let (i, r) = joined.map_err(|e| S1Error::InvalidRequest(e.to_string()))?;
        // Returning drops the set, which stops the other downloads.
        fetched[i] = Some(r?);
    }
    let mut next = fetched.into_iter().flatten();
    let messages = v
        .get_mut("input")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten();
    for m in messages {
        let parts = m
            .get_mut("content")
            .and_then(Value::as_array_mut)
            .into_iter()
            .flatten();
        for p in parts {
            let is_image = p.get("type").and_then(Value::as_str) == Some("input_image");
            let Some(slot) = p.get_mut("image_url").filter(|_| is_image) else {
                continue;
            };
            if slot.as_str().is_some_and(is_url)
                && let Some(data) = next.next()
            {
                *slot = Value::String(data);
            }
        }
    }
    let out = serde_json::to_vec(&v).expect("JSON");
    if out.len() > max_body {
        return Err(too_long());
    }
    Ok(Some(Bytes::from(out)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::routing::get;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n0000";
    const LIMIT: usize = 64 * 1024 * 1024;

    async fn local(body: &[u8]) -> Result<Option<Bytes>, S1Error> {
        inline_image_urls(body, FetchPolicy::ALLOW_LOCAL, LIMIT).await
    }

    async fn server() -> std::net::SocketAddr {
        let app = Router::new()
            .route("/a.png", get(|| async { PNG.to_vec() }))
            .route("/a.gif", get(|| async { b"GIF89a".to_vec() }))
            .route(
                "/missing",
                get(|| async { (axum::http::StatusCode::NOT_FOUND, "no") }),
            );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        addr
    }

    fn req(urls: &[String]) -> Vec<u8> {
        let parts: Vec<Value> = urls
            .iter()
            .map(|u| serde_json::json!({"type": "input_image", "image_url": u}))
            .chain([serde_json::json!({"type": "input_text", "text": "hi"})])
            .collect();
        serde_json::to_vec(&serde_json::json!({
            "model": "decide",
            "input": [{"role": "user", "content": parts}],
            "questions": [{"type": "predicate", "instructions": "?"}]
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn urls_become_data_urls() {
        let addr = server().await;
        let urls = vec![
            format!("http://{addr}/a.png"),
            "data:image/png;base64,AAAA".to_string(),
        ];
        let out = local(&req(&urls)).await.unwrap().unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();
        let parts = v["input"][0]["content"].as_array().unwrap();
        let b64 = base64::engine::general_purpose::STANDARD.encode(PNG);
        assert_eq!(
            parts[0]["image_url"],
            format!("data:image/png;base64,{b64}")
        );
        assert_eq!(parts[1]["image_url"], "data:image/png;base64,AAAA");
        assert_eq!(parts[2]["text"], "hi");
        // Nothing to fetch: unchanged.
        assert!(
            local(&req(&["data:image/png;base64,AAAA".into()]))
                .await
                .unwrap()
                .is_none()
        );
        assert!(local(b"{\"input\":\"x\"}").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn bad_urls_fail_clearly() {
        let addr = server().await;
        let e = local(&req(&[format!("http://{addr}/missing")]))
            .await
            .unwrap_err();
        assert_eq!(e.code(), "invalid_request");
        assert!(e.to_string().contains("404"), "{e}");
        let e = local(&req(&[format!("http://{addr}/a.gif")]))
            .await
            .unwrap_err();
        assert_eq!(e.code(), "unsupported");
    }

    // Review B3: errors name the image by position, not by URL.
    #[tokio::test]
    async fn errors_do_not_show_the_url() {
        let addr = server().await;
        let e = local(&req(&[format!("http://{addr}/missing?sig=SECRET")]))
            .await
            .unwrap_err();
        let m = e.to_string();
        assert!(m.contains("input[0].content[0]"), "{m}");
        assert!(!m.contains("SECRET") && !m.contains("http"), "{m}");
        let e = local(&req(&[format!("http://{addr}/a.gif?sig=SECRET")]))
            .await
            .unwrap_err();
        assert!(!e.to_string().contains("SECRET"), "{e}");
    }

    // Review B1: by default only public HTTPS URLs are fetched; the check
    // happens before any connection.
    #[tokio::test]
    async fn default_policy_refuses_local_and_plain_http_urls() {
        let addr = server().await;
        let port = addr.port();
        for url in [
            format!("http://{addr}/a.png"),
            format!("https://127.0.0.1:{port}/a.png"),
            format!("https://localhost:{port}/a.png"),
            format!("https://[::1]:{port}/a.png"),
            "https://169.254.169.254/latest/meta-data".to_string(),
            "https://10.0.0.1/a.png".to_string(),
            "https://[::ffff:127.0.0.1]/a.png".to_string(),
            "ftp://example.com/a.png".to_string(),
        ] {
            let body = serde_json::to_vec(&serde_json::json!({
                "input": [{"role": "user", "content": [{"type": "input_image", "image_url": url}]}]
            }))
            .unwrap();
            let r = inline_image_urls(&body, FetchPolicy::PUBLIC_HTTPS, LIMIT).await;
            if url.starts_with("ftp") {
                assert!(r.unwrap().is_none(), "not an HTTP(S) URL: left alone");
                continue;
            }
            let e = r.unwrap_err();
            assert_eq!(e.code(), "invalid_request", "{url}: {e}");
        }
    }

    #[test]
    fn redirect_targets_are_checked_like_the_first_url() {
        let ok = |u: &str| check_target(&Url::parse(u).unwrap(), FetchPolicy::PUBLIC_HTTPS);
        assert!(ok("https://example.com/a.png").is_ok());
        assert!(ok("https://93.184.216.34/a.png").is_ok());
        assert!(ok("http://example.com/a.png").is_err());
        assert!(ok("https://127.0.0.1/a.png").is_err());
        assert!(ok("https://[fe80::1]/a.png").is_err());
        assert!(ok("https://[fd00::1]/a.png").is_err());
        assert!(ok("https://192.168.1.1/").is_err());
        let local = |u: &str| check_target(&Url::parse(u).unwrap(), FetchPolicy::ALLOW_LOCAL);
        assert!(local("http://127.0.0.1/a.png").is_ok());
        assert!(local("file:///etc/passwd").is_err());
    }

    // Review R2 (cheap): the redirect rule is a named function.
    #[test]
    fn public_redirects_are_checked_and_limited() {
        let url = |u: &str| Url::parse(u).unwrap();
        assert!(public_redirect(&url("https://example.com/a.png"), 1).is_ok());
        assert!(public_redirect(&url("https://example.com/a.png"), MAX_REDIRECTS).is_ok());
        let e = public_redirect(&url("https://example.com/a.png"), MAX_REDIRECTS + 1).unwrap_err();
        assert!(e.contains("too many redirects"), "{e}");
        for bad in [
            "http://example.com/a.png",
            "https://127.0.0.1/a.png",
            "https://169.254.169.254/latest",
            "https://[::1]/a.png",
            "file:///etc/passwd",
        ] {
            let e = public_redirect(&url(bad), 1).unwrap_err();
            assert!(e.starts_with("redirect refused"), "{bad}: {e}");
        }
    }

    // The public client really applies the rule: a redirect to a loopback
    // address is refused (the first URL is an IP literal, so no DNS check).
    #[tokio::test]
    async fn public_client_refuses_redirects_to_local_addresses() {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let app = Router::new()
            .route(
                "/hop",
                get(move || async move {
                    axum::response::Redirect::temporary(&format!("http://{addr}/a.png"))
                }),
            )
            .route("/a.png", get(|| async { PNG.to_vec() }));
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        let e = public_client()
            .get(format!("http://{addr}/hop"))
            .send()
            .await
            .expect_err("redirect to loopback must be refused");
        assert!(e.is_redirect(), "{e:?}");
        assert!(describe(e).contains("redirect refused"));
    }

    #[test]
    fn public_addresses() {
        let p = |s: &str| is_public(s.parse().unwrap());
        for ok in [
            "8.8.8.8",
            "93.184.216.34",
            "2606:4700::1111",
            "2002:808:808::1",
        ] {
            assert!(p(ok), "{ok}");
        }
        for bad in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.0.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "255.255.255.255",
            "224.0.0.1",
            "192.0.2.1",
            "198.18.0.1",
            "240.0.0.1",
            "192.0.0.8",
            "::1",
            "::",
            "fe80::1",
            "fc00::1",
            "ff02::1",
            "::ffff:10.0.0.1",
            "64:ff9b::a00:1",
            "2001:db8::1",
            "2001::1",
            "2002:7f00:1::1",
            "::127.0.0.1",
        ] {
            assert!(!p(bad), "{bad}");
        }
    }

    // Review B4: an escaped key still counts as `image_url`.
    #[tokio::test]
    async fn escaped_image_url_key_is_seen() {
        let addr = server().await;
        let body = format!(
            r#"{{"model":"decide","input":[{{"role":"user","content":[{{"type":"input_image","image\u005furl":"http://{addr}/a.png"}}]}}],"questions":[]}}"#
        );
        assert!(has_image_urls(body.as_bytes()));
        let out = local(body.as_bytes()).await.unwrap().expect("fetched");
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert!(
            v["input"][0]["content"][0]["image_url"]
                .as_str()
                .unwrap()
                .starts_with("data:image/png;base64,")
        );
        let plain = br#"{"input":"no images \u00e9"}"#;
        assert!(!has_image_urls(plain));
    }

    // Review B2: all images of a request share the body budget, enforced
    // while downloading, and downloads in flight are bounded.
    #[tokio::test]
    async fn images_share_the_body_budget() {
        let big: Vec<u8> = PNG
            .iter()
            .copied()
            .chain(std::iter::repeat_n(0u8, 400_000))
            .collect();
        let app = Router::new().route("/big.png", get(move || async move { big.clone() }));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        let urls: Vec<String> = (0..4)
            .map(|i| format!("http://{addr}/big.png?{i}"))
            .collect();
        let body = req(&urls);
        // Room for about two images, not four.
        let e = inline_image_urls(&body, FetchPolicy::ALLOW_LOCAL, 1_200_000)
            .await
            .unwrap_err();
        assert_eq!(e.code(), "too_long", "{e}");
        // Enough room: all four inline, and the result fits the limit.
        let out = inline_image_urls(&body, FetchPolicy::ALLOW_LOCAL, 2_400_000)
            .await
            .unwrap()
            .unwrap();
        assert!(out.len() <= 2_400_000);
    }

    // Review R2 (cheap): an endless image stops downloading once the
    // shared budget is spent, long before the per-image limit.
    #[tokio::test]
    async fn downloads_stop_when_the_shared_budget_runs_out() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let served = Arc::new(AtomicUsize::new(0));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let s = served.clone();
        tokio::spawn(async move {
            loop {
                let (mut sock, _) = l.accept().await.unwrap();
                let s = s.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let _ = sock.read(&mut buf).await;
                    let head =
                        b"HTTP/1.1 200 OK\r\ncontent-type: image/png\r\nconnection: close\r\n\r\n";
                    if sock.write_all(head).await.is_err() || sock.write_all(PNG).await.is_err() {
                        return;
                    }
                    let chunk = vec![0u8; 16 * 1024];
                    // Endless, paced so socket buffers stay small.
                    while sock.write_all(&chunk).await.is_ok() {
                        s.fetch_add(chunk.len(), Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                });
            }
        });
        let urls: Vec<String> = (0..2)
            .map(|i| format!("http://{addr}/endless.png?{i}"))
            .collect();
        let r = tokio::time::timeout(
            Duration::from_secs(20),
            inline_image_urls(&req(&urls), FetchPolicy::ALLOW_LOCAL, 200_000),
        )
        .await
        .expect("the download must stop");
        let at_stop = served.load(Ordering::SeqCst);
        assert_eq!(r.unwrap_err().code(), "too_long");
        assert!(
            at_stop < 4 * 1024 * 1024,
            "{at_stop} bytes served before the stop"
        );
    }

    #[tokio::test]
    async fn downloads_in_flight_are_bounded() {
        use std::sync::atomic::AtomicUsize;
        let now = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let (n, p) = (now.clone(), peak.clone());
        let app = Router::new().route(
            "/slow.png",
            get(move || {
                let (n, p) = (n.clone(), p.clone());
                async move {
                    let c = n.fetch_add(1, Ordering::SeqCst) + 1;
                    p.fetch_max(c, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    n.fetch_sub(1, Ordering::SeqCst);
                    PNG.to_vec()
                }
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        let urls: Vec<String> = (0..40)
            .map(|i| format!("http://{addr}/slow.png?{i}"))
            .collect();
        local(&req(&urls)).await.unwrap().unwrap();
        let max = peak.load(Ordering::SeqCst);
        assert!(max <= MAX_CONCURRENT_FETCHES, "{max} downloads at once");
    }
}
