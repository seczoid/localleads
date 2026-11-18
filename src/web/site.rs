//! How the server is reached, for self-hosting: the public URL, whether a
//! reverse proxy sits in front, the one-time setup code, the sign-in rate
//! limit and the security headers on every response.

use axum::extract::{ConnectInfo, Request};
use axum::http::{HeaderMap, HeaderValue, header};
use axum::middleware::Next;
use axum::response::Response;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

#[derive(Debug, Default)]
pub struct Site {
    /// `https://leads.example.com`, without a trailing slash.
    pub public_url: Option<String>,
    /// Host name of the public URL, lower-cased, without the port.
    pub public_host: Option<String>,
    /// Trust `X-Forwarded-For` and `X-Forwarded-Proto` from the proxy.
    pub behind_proxy: bool,
    /// Needed to create the account when the server is reachable from
    /// other machines, so a stranger can't claim a fresh install.
    pub setup_code: Option<String>,
}

static SITE: OnceLock<Site> = OnceLock::new();

pub fn configure(site: Site) {
    let _ = SITE.set(site);
}

pub fn site() -> &'static Site {
    SITE.get_or_init(Site::default)
}

impl Site {
    pub fn https(&self) -> bool {
        self.public_url
            .as_deref()
            .is_some_and(|url| url.starts_with("https://"))
    }

    /// Whether this request reached us over HTTPS.
    pub fn request_is_https(&self, headers: &HeaderMap) -> bool {
        self.https()
            || (self.behind_proxy
                && headers
                    .get("x-forwarded-proto")
                    .and_then(|value| value.to_str().ok())
                    .is_some_and(|proto| proto.eq_ignore_ascii_case("https")))
    }

    /// The address of the person making the request: the proxy's
    /// `X-Forwarded-For` (the hop it added, so clients can't spoof it) when
    /// behind one, otherwise the TCP peer.
    pub fn client_ip(&self, headers: &HeaderMap, extensions: &axum::http::Extensions) -> Option<IpAddr> {
        if self.behind_proxy
            && let Some(ip) = headers
                .get("x-forwarded-for")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.rsplit(',').next())
                .and_then(|value| value.trim().parse().ok())
        {
            return Some(ip);
        }
        extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|info| info.0.ip())
    }
}

/// The requester's address, as [`Site::client_ip`] works it out.
pub struct ClientIp(pub Option<IpAddr>);

impl<S: Send + Sync> axum::extract::FromRequestParts<S> for ClientIp {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut axum::http::request::Parts, _: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(site().client_ip(&parts.headers, &parts.extensions)))
    }
}

/// Host name of `host` (a Host header or URL authority), without the port.
pub fn host_name(host: &str) -> String {
    let name = if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or_default()
    } else {
        host.rsplit_once(':').map_or(host, |(name, _)| name)
    };
    name.to_ascii_lowercase()
}

// ------------------------------------------------------- rate limit

/// Failed sign-ins (and setup codes, and current-password checks) allowed
/// per address in a window, and across all addresses.
const PER_ADDRESS: u32 = 10;
const OVERALL: u32 = 100;
const WINDOW: Duration = Duration::from_secs(15 * 60);

#[derive(Default)]
struct Failures {
    by_address: HashMap<IpAddr, (u32, Instant)>,
    overall: (u32, Option<Instant>),
}

fn failures() -> &'static Mutex<Failures> {
    static FAILURES: OnceLock<Mutex<Failures>> = OnceLock::new();
    FAILURES.get_or_init(Mutex::default)
}

/// How long until this address may try again, if it's locked out.
pub fn locked_out(ip: Option<IpAddr>) -> Option<Duration> {
    let failures = failures().lock().ok()?;
    let now = Instant::now();
    if let (count, Some(start)) = failures.overall
        && count >= OVERALL
        && now.duration_since(start) < WINDOW
    {
        return Some(WINDOW - now.duration_since(start));
    }
    let (count, start) = *failures.by_address.get(&ip?)?;
    (count >= PER_ADDRESS && now.duration_since(start) < WINDOW).then(|| WINDOW - now.duration_since(start))
}

pub fn record_failure(ip: Option<IpAddr>) {
    let Ok(mut failures) = failures().lock() else {
        return;
    };
    let now = Instant::now();
    let overall = &mut failures.overall;
    match overall.1 {
        Some(start) if now.duration_since(start) < WINDOW => overall.0 += 1,
        _ => *overall = (1, Some(now)),
    }
    if let Some(ip) = ip {
        let entry = failures.by_address.entry(ip).or_insert((0, now));
        if now.duration_since(entry.1) >= WINDOW {
            *entry = (0, now);
        }
        entry.0 += 1;
        if failures.by_address.len() > 10_000 {
            failures
                .by_address
                .retain(|_, (_, start)| now.duration_since(*start) < WINDOW);
        }
    }
}

pub fn clear_failures(ip: Option<IpAddr>) {
    if let (Ok(mut failures), Some(ip)) = (failures().lock(), ip) {
        failures.by_address.remove(&ip);
    }
}

/// "14 minutes" for a lockout message.
pub fn minutes(wait: Duration) -> String {
    let minutes = wait.as_secs().div_ceil(60).max(1);
    if minutes == 1 { "a minute".to_owned() } else { format!("{minutes} minutes") }
}

// ---------------------------------------------------------- headers

/// Images on lead pages come from Google and Gravatar; scripts, styles and
/// fonts are served by the app, except GitHub's star button in the sidebar
/// (its script from buttons.github.io, its star count from api.github.com).
/// Alpine and htmx evaluate attribute expressions, which needs 'unsafe-eval'.
const CSP: &str = "default-src 'self'; \
    script-src 'self' 'unsafe-inline' 'unsafe-eval' https://buttons.github.io https://api.github.com; \
    style-src 'self' 'unsafe-inline'; img-src 'self' data: https:; font-src 'self'; \
    connect-src 'self' https://api.github.com; object-src 'none'; base-uri 'self'; form-action 'self'; frame-ancestors 'none'";

pub async fn security_headers(request: Request, next: Next) -> Response {
    let is_static = request.uri().path().starts_with("/static/");
    let https = site().request_is_https(request.headers());
    // No HSTS for localhost or a bare IP address: those use self-signed
    // certificates, and HSTS would make the browser's warning a dead end.
    let local = site().public_host.as_deref().is_none_or(|host| {
        host == "localhost"
            || host.ends_with(".localhost")
            || host.trim_start_matches('[').trim_end_matches(']').parse::<IpAddr>().is_ok()
    });
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    headers.insert("x-frame-options", HeaderValue::from_static("DENY"));
    headers.insert("referrer-policy", HeaderValue::from_static("same-origin"));
    headers.insert("content-security-policy", HeaderValue::from_static(CSP));
    headers.insert(
        "permissions-policy",
        HeaderValue::from_static("camera=(), microphone=(), geolocation=(), payment=()"),
    );
    if https && !local {
        headers.insert(
            header::STRICT_TRANSPORT_SECURITY,
            HeaderValue::from_static("max-age=31536000"),
        );
    }
    // Lead data and keys shouldn't sit in shared or browser caches.
    if !is_static && !headers.contains_key(header::CACHE_CONTROL) {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_names_drop_the_port() {
        assert_eq!(host_name("Leads.Example.com:443"), "leads.example.com");
        assert_eq!(host_name("[::1]:7878"), "::1");
        assert_eq!(host_name("localhost"), "localhost");
    }

    #[test]
    fn addresses_get_locked_out_after_failures() {
        let ip: Option<IpAddr> = Some("203.0.113.9".parse().unwrap());
        for _ in 0..PER_ADDRESS {
            assert!(locked_out(ip).is_none());
            record_failure(ip);
        }
        assert!(locked_out(ip).is_some());
        clear_failures(ip);
        assert!(locked_out(ip).is_none());
    }
}
