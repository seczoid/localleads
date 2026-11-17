//! Email verification with check-if-email-exists (Reacher): syntax, MX
//! records, the mailbox itself over SMTP, catch-all, disposable, role and
//! free-provider checks, and Gravatar.
//!
//! SMTP checks go out through the search's SOCKS5 proxies when it has any
//! (HTTP proxies can't carry SMTP). Without one they go direct, if this
//! network lets port 25 out; otherwise the mailbox step is skipped and the
//! address keeps its DNS and syntax result.

use super::run::Reach;
use crate::models::ProxyEntry;
use check_if_email_exists::{
    CheckEmailInput, CheckEmailInputProxy, CheckEmailOutput, Reachable, check_email,
    smtp::verif_method::{GmailVerifMethod, HotmailB2CVerifMethod, VerifMethod, YahooVerifMethod},
};
use serde::Serialize;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// Everything the check found out about one address.
#[derive(Clone, Debug, Default, Serialize, serde::Deserialize)]
pub struct EmailCheck {
    /// safe, risky, unknown or invalid.
    pub reachable: String,
    /// Outscraper's wording: RECEIVING, INVALID or UNKNOWN.
    pub status: String,
    /// Why: "SMTP validated", "Role", "Catch-all", "Invalid DNS", …
    pub status_details: String,
    pub valid_syntax: bool,
    pub accepts_mail: bool,
    pub mx_hosts: Vec<String>,
    pub smtp_checked: bool,
    pub can_connect_smtp: bool,
    pub is_deliverable: bool,
    pub is_disabled: bool,
    pub has_full_inbox: bool,
    pub is_catch_all: bool,
    pub is_disposable: bool,
    pub is_role: bool,
    /// Gmail, Yahoo and the like rather than a company domain.
    pub is_free: bool,
    pub gravatar_url: Option<String>,
    /// A likely typo fix Reacher suggests for a bad domain.
    pub suggestion: Option<String>,
    pub error: Option<String>,
    /// The mail server's own answer when we asked it a second time about
    /// an address the first check called missing.
    pub smtp_reply: Option<String>,
}

pub struct Verifier {
    socks: Vec<CheckEmailInputProxy>,
    next: AtomicUsize,
    /// Direct SMTP works from here (port 25 isn't blocked).
    direct_smtp: bool,
    pub gravatar: bool,
}

impl Verifier {
    /// Checks once whether port 25 gets out, so a blocked network doesn't
    /// spend a timeout on every address.
    pub async fn new(proxies: &[ProxyEntry], gravatar: bool) -> Self {
        let candidates: Vec<CheckEmailInputProxy> = proxies
            .iter()
            .filter(|proxy| proxy.protocol.starts_with("socks5"))
            .filter_map(socks_proxy)
            .take(MAX_PROBED)
            .collect();
        // Many SOCKS proxies refuse port 25; only keep ones that reach a
        // mail server, so checks don't all fail on the proxy.
        let probes = candidates.into_iter().map(|proxy| async move {
            socks_reaches_smtp(&proxy).await.then_some(proxy)
        });
        let socks: Vec<CheckEmailInputProxy> =
            futures::future::join_all(probes).await.into_iter().flatten().collect();
        let direct_smtp = socks.is_empty() && port_25_open().await;
        Self {
            socks,
            next: AtomicUsize::new(0),
            direct_smtp,
            gravatar,
        }
    }

    pub fn proxy_count(&self) -> usize {
        self.socks.len()
    }

    /// Whether mailbox checks go through SOCKS5 proxies.
    pub fn via_proxies(&self) -> bool {
        !self.socks.is_empty()
    }

    /// Whether mailboxes can be checked at all (proxy or open port 25).
    pub fn smtp_available(&self) -> bool {
        !self.socks.is_empty() || self.direct_smtp
    }

    pub async fn check(&self, email: &str) -> EmailCheck {
        let proxy = (!self.socks.is_empty())
            .then(|| self.socks[self.next.fetch_add(1, Ordering::Relaxed) % self.socks.len()].clone());
        if !self.smtp_available() {
            return self.check_without_smtp(email).await;
        }
        let mut verif_method = VerifMethod::new_with_same_config_for_all(
            proxy.clone(),
            "gmail.com".to_owned(),
            "reacher@gmail.com".to_owned(),
            25,
            Some(Duration::from_secs(12)),
            1,
        );
        // Reacher's defaults for these need a headless Chrome. Yahoo and AOL
        // mailboxes are checked with Yahoo's sign-up API instead, Outlook.com
        // and Hotmail over SMTP like everything else.
        verif_method.yahoo = YahooVerifMethod::Api;
        let GmailVerifMethod::Smtp(smtp) = &verif_method.gmail;
        verif_method.hotmailb2c = HotmailB2CVerifMethod::Smtp(smtp.clone());
        let input = CheckEmailInput {
            to_email: email.to_owned(),
            verif_method,
            check_gravatar: self.gravatar,
            ..Default::default()
        };
        let output = tokio::time::timeout(Duration::from_secs(45), check_email(&input)).await;
        match output {
            Ok(output) => {
                let check = from_output(&output, true);
                if check.status_details == "Mailbox not found" {
                    return self.confirm_missing(email, check, proxy.as_ref()).await;
                }
                check
            }
            Err(_) => EmailCheck {
                reachable: Reach::Unknown.as_str().to_owned(),
                status: "UNKNOWN".to_owned(),
                status_details: "Timed out".to_owned(),
                error: Some("verification timed out".to_owned()),
                ..EmailCheck::default()
            },
        }
    }

    /// Asks the mail server again, on a fresh connection, before calling an
    /// address missing. Reacher only keeps a yes/no, and it reads any
    /// permanent 5.7.1 as "no such mailbox", but servers also send that
    /// (and Microsoft 365 sends it at random) when they refuse the
    /// checking IP. The real address goes first, then a made-up one to
    /// tell a catch-all apart.
    async fn confirm_missing(
        &self,
        email: &str,
        mut check: EmailCheck,
        proxy: Option<&CheckEmailInputProxy>,
    ) -> EmailCheck {
        let Some(host) = check.mx_hosts.first().map(|host| host.trim_end_matches('.').to_owned()) else {
            return check;
        };
        let mut answer = None;
        for _ in 0..2 {
            let probe =
                tokio::time::timeout(Duration::from_secs(30), probe_rcpt(proxy, &host, email)).await;
            let Ok(Ok(probe)) = probe else { break };
            let blocked = probe.verdict() == ProbeVerdict::Blocked;
            answer = Some(probe);
            if !blocked {
                break;
            }
        }
        let Some(probe) = answer else {
            return check;
        };
        check.smtp_reply = Some(probe.real.clone());
        match probe.verdict() {
            ProbeVerdict::Missing => check,
            ProbeVerdict::Blocked => {
                check.smtp_checked = false;
                check.is_deliverable = false;
                check.error = Some(probe.real);
                let check = finish(check, Reach::Unknown);
                EmailCheck {
                    status_details: "Mail server refused the check (sender IP blocked)".to_owned(),
                    ..check
                }
            }
            ProbeVerdict::Unclear => {
                let later = probe.real.starts_with('4');
                check.smtp_checked = false;
                check.is_deliverable = false;
                check.error = Some(probe.real);
                let check = finish(check, Reach::Unknown);
                let details = if later {
                    "Mail server asked to try later"
                } else {
                    "Cannot validate"
                };
                EmailCheck { status_details: details.to_owned(), ..check }
            }
            ProbeVerdict::Accepted { catch_all } => {
                check.is_deliverable = true;
                check.is_catch_all = catch_all;
                let reach = if catch_all || check.is_role || check.is_disposable {
                    Reach::Risky
                } else {
                    Reach::Safe
                };
                finish(check, reach)
            }
        }
    }

    /// Syntax, MX and misc only, for networks that block port 25.
    async fn check_without_smtp(&self, email: &str) -> EmailCheck {
        use check_if_email_exists::{misc::check_misc, mx::check_mx, syntax::check_syntax};
        let syntax = check_syntax(email);
        let mut check = EmailCheck {
            valid_syntax: syntax.is_valid_syntax,
            ..EmailCheck::default()
        };
        if !syntax.is_valid_syntax {
            return finish(check, Reach::Invalid);
        }
        match check_mx(&syntax).await {
            Ok(mx) => {
                check.accepts_mail = mx.accepts_mail && mx.lookup.is_ok();
                if let Ok(lookup) = &mx.lookup {
                    check.mx_hosts = lookup.iter().map(|host| host.exchange().to_string()).collect();
                }
            }
            Err(error) => check.error = Some(format!("{error:?}")),
        }
        if !check.accepts_mail {
            return finish(check, Reach::Invalid);
        }
        let misc = check_misc(&syntax, self.gravatar, None).await;
        check.is_disposable = misc.is_disposable;
        check.is_role = misc.is_role_account || super::emails::is_role(email);
        check.is_free = misc.is_b2c;
        check.gravatar_url = misc.gravatar_url;
        let reach = if check.is_disposable { Reach::Risky } else { Reach::Unknown };
        finish(check, reach)
    }
}

fn from_output(output: &CheckEmailOutput, smtp_checked: bool) -> EmailCheck {
    let mut check = EmailCheck {
        valid_syntax: output.syntax.is_valid_syntax,
        suggestion: output.syntax.suggestion.clone(),
        smtp_checked,
        ..EmailCheck::default()
    };
    match &output.mx {
        Ok(mx) => {
            check.accepts_mail = mx.accepts_mail && mx.lookup.is_ok();
            if let Ok(lookup) = &mx.lookup {
                check.mx_hosts = lookup.iter().map(|host| host.exchange().to_string()).collect();
            }
        }
        Err(error) => check.error = Some(format!("{error:?}")),
    }
    if let Ok(misc) = &output.misc {
        check.is_disposable = misc.is_disposable;
        check.is_role = misc.is_role_account || super::emails::is_role(&output.input);
        check.is_free = misc.is_b2c;
        check.gravatar_url = misc.gravatar_url.clone();
    }
    match &output.smtp {
        Ok(smtp) => {
            check.can_connect_smtp = smtp.can_connect_smtp;
            check.is_deliverable = smtp.is_deliverable;
            check.is_disabled = smtp.is_disabled;
            check.has_full_inbox = smtp.has_full_inbox;
            check.is_catch_all = smtp.is_catch_all;
        }
        Err(error) => {
            check.smtp_checked = false;
            check.error.get_or_insert_with(|| format!("{error:?}"));
        }
    }
    let reach = match output.is_reachable {
        Reachable::Safe => Reach::Safe,
        Reachable::Risky => Reach::Risky,
        Reachable::Invalid => Reach::Invalid,
        Reachable::Unknown => Reach::Unknown,
    };
    let check = finish(check, reach);
    // Reacher calls a role address "risky" before looking at the mailbox;
    // a mailbox the server says doesn't exist is invalid either way.
    if check.status == "INVALID" && reach != Reach::Invalid {
        return EmailCheck {
            reachable: Reach::Invalid.as_str().to_owned(),
            ..check
        };
    }
    check
}

/// The mail server turned the check away because of where it came from
/// (a blocklisted or residential IP, a country block), not the address.
fn refused_sender(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    [
        "spamhaus", "blocklist", "blacklist", "block list", "black list", "not allowed to connect",
        "rbl", "dnsbl", "barracuda", "spamcop", "sorbs", "client host rejected", "ip address",
        "poor reputation", "dynamic ip", "residential", "policy", "access denied",
    ]
    .iter()
    .any(|signal| error.contains(signal))
}

/// Fills in the verdict and Outscraper's status wording.
fn finish(mut check: EmailCheck, reach: Reach) -> EmailCheck {
    check.reachable = reach.as_str().to_owned();
    let (status, details) = if !check.valid_syntax {
        ("INVALID", "Invalid syntax")
    } else if !check.accepts_mail {
        ("INVALID", "Invalid DNS")
    } else if check.smtp_checked && check.is_disabled {
        ("INVALID", "Mailbox disabled")
    } else if check.smtp_checked && check.can_connect_smtp && !check.is_deliverable && !check.is_catch_all {
        ("INVALID", "Mailbox not found")
    } else if check.is_disposable {
        ("RECEIVING", "Disposable")
    } else if check.smtp_checked && check.has_full_inbox {
        ("RECEIVING", "Full inbox")
    } else if check.smtp_checked && check.is_catch_all {
        ("RECEIVING", "Catch-all")
    } else if check.smtp_checked && check.is_deliverable && check.is_role {
        ("RECEIVING", "Role")
    } else if check.smtp_checked && check.is_deliverable {
        ("RECEIVING", "SMTP validated")
    } else if check.smtp_checked {
        ("UNKNOWN", "Cannot validate")
    } else if check.error.as_deref().is_some_and(refused_sender) {
        ("UNKNOWN", "Mail server refused the check (sender IP blocked)")
    } else if check.is_role {
        ("UNKNOWN", "Role, mailbox not checked")
    } else {
        ("UNKNOWN", "Domain accepts mail, mailbox not checked")
    };
    check.status = status.to_owned();
    check.status_details = details.to_owned();
    check
}

fn socks_proxy(proxy: &ProxyEntry) -> Option<CheckEmailInputProxy> {
    let url = url::Url::parse(&proxy.url).ok()?;
    let decode = |value: &str| {
        url::form_urlencoded::parse(format!("x={value}").as_bytes())
            .next()
            .map(|(_, value)| value.into_owned())
    };
    let username = proxy
        .username
        .clone()
        .or_else(|| (!url.username().is_empty()).then(|| decode(url.username())).flatten());
    let password = proxy
        .password
        .clone()
        .or_else(|| url.password().and_then(decode));
    Some(CheckEmailInputProxy {
        host: url.host_str()?.to_owned(),
        port: url.port().unwrap_or(1080),
        username,
        password,
        timeout_ms: Some(10_000),
    })
}

/// What a mail server said to RCPT TO for the address and a made-up one.
struct Probe {
    real: String,
    random: String,
}

#[derive(Debug, PartialEq, Eq)]
enum ProbeVerdict {
    Accepted { catch_all: bool },
    /// The server says the mailbox doesn't exist.
    Missing,
    /// The server turned away the checking IP, not the address.
    Blocked,
    /// A temporary or unexplained refusal.
    Unclear,
}

impl Probe {
    fn verdict(&self) -> ProbeVerdict {
        let code = |reply: &str| reply.get(..3).and_then(|code| code.parse::<u16>().ok()).unwrap_or(0);
        let real = code(&self.real);
        if (200..300).contains(&real) {
            return ProbeVerdict::Accepted { catch_all: (200..300).contains(&code(&self.random)) };
        }
        if ip_refused(&self.real) {
            return ProbeVerdict::Blocked;
        }
        if (500..600).contains(&real) && mailbox_missing(&self.real) {
            return ProbeVerdict::Missing;
        }
        ProbeVerdict::Unclear
    }
}

/// A reply that refuses the checking IP or the session rather than the
/// mailbox. Microsoft's "5.4.1 Recipient address rejected: Access denied"
/// is its answer for an unknown recipient, so that one isn't a block.
fn ip_refused(reply: &str) -> bool {
    let lower = reply.to_ascii_lowercase();
    let lower = lower.replace("recipient address rejected: access denied", "");
    refused_sender(&lower)
        || ["client host", "blocked", "service unavailable", "reputation"]
            .iter()
            .any(|signal| lower.contains(signal))
}

fn mailbox_missing(reply: &str) -> bool {
    let lower = reply.to_ascii_lowercase();
    lower.contains("5.1.1")
        || lower.contains("5.1.10")
        || lower.contains("5.4.1")
        || lower.contains("5.2.1")
        || [
            "user unknown", "unknown user", "no such user", "does not exist", "doesn't exist", "not found",
            "unknown recipient", "recipient unknown", "invalid recipient", "recipient rejected",
            "address rejected", "no mailbox", "mailbox unavailable", "mailbox not found", "undeliverable",
            "no such recipient", "invalid mailbox", "not a valid mailbox", "unrouteable",
        ]
        .iter()
        .any(|signal| lower.contains(signal))
}

/// One SMTP session: EHLO, MAIL FROM, then RCPT TO for the address and for
/// a random one at the same domain. Nothing is ever sent.
async fn probe_rcpt(proxy: Option<&CheckEmailInputProxy>, host: &str, email: &str) -> std::io::Result<Probe> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let stream = match proxy {
        Some(proxy) => socks_connect(proxy, host, 25)
            .await?
            .ok_or_else(|| std::io::Error::other("the proxy refused the connection"))?,
        None => tokio::net::TcpStream::connect((host, 25)).await?,
    };
    let mut stream = BufReader::new(stream);
    async fn reply(stream: &mut BufReader<tokio::net::TcpStream>) -> std::io::Result<String> {
        let mut text = String::new();
        loop {
            let mut line = String::new();
            if stream.read_line(&mut line).await? == 0 {
                return Err(std::io::Error::other("the mail server closed the connection"));
            }
            let line = line.trim_end();
            let last = line.as_bytes().get(3) != Some(&b'-');
            if text.is_empty() || last {
                text = line.to_owned();
            }
            if last {
                return Ok(text);
            }
        }
    }
    async fn say(stream: &mut BufReader<tokio::net::TcpStream>, line: &str) -> std::io::Result<String> {
        stream.get_mut().write_all(format!("{line}\r\n").as_bytes()).await?;
        reply(stream).await
    }
    let greeting = reply(&mut stream).await?;
    if !greeting.starts_with('2') {
        return Ok(Probe { real: greeting.clone(), random: greeting });
    }
    let ehlo = say(&mut stream, "EHLO gmail.com").await?;
    if !ehlo.starts_with('2') {
        say(&mut stream, "HELO gmail.com").await?;
    }
    let from = say(&mut stream, "MAIL FROM:<reacher@gmail.com>").await?;
    if !from.starts_with('2') {
        return Ok(Probe { real: from.clone(), random: from });
    }
    let real = say(&mut stream, &format!("RCPT TO:<{email}>")).await?;
    let domain = email.rsplit_once('@').map_or("", |(_, domain)| domain);
    let random = say(&mut stream, &format!("RCPT TO:<{}@{domain}>", random_local())).await.unwrap_or_default();
    let _ = say(&mut stream, "QUIT").await;
    Ok(Probe { real, random })
}

/// A local part nobody has: 15 letters and digits.
fn random_local() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
    let mut seed = hasher.finish();
    (0..15)
        .map(|_| {
            let digit = (seed % 36) as u32;
            seed = (seed / 36) ^ seed.rotate_left(17).wrapping_mul(0x9E37_79B9_7F4A_7C15);
            char::from_digit(digit, 36).unwrap_or('x')
        })
        .collect()
}

/// SOCKS5 proxies tried for SMTP at most.
const MAX_PROBED: usize = 25;

/// Whether this SOCKS5 proxy opens a connection to a mail server's port 25.
async fn socks_reaches_smtp(proxy: &CheckEmailInputProxy) -> bool {
    tokio::time::timeout(Duration::from_secs(8), socks_connect_25(proxy))
        .await
        .is_ok_and(|result| result.unwrap_or(false))
}

async fn socks_connect_25(proxy: &CheckEmailInputProxy) -> std::io::Result<bool> {
    Ok(socks_connect(proxy, "gmail-smtp-in.l.google.com", 25).await?.is_some())
}

/// Opens a tunnel through a SOCKS5 proxy; `None` when the proxy refuses.
async fn socks_connect(
    proxy: &CheckEmailInputProxy,
    host: &str,
    port: u16,
) -> std::io::Result<Option<tokio::net::TcpStream>> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect((proxy.host.as_str(), proxy.port)).await?;
    let credentials = proxy.username.as_deref().zip(proxy.password.as_deref());
    let greeting: &[u8] = if credentials.is_some() { &[5, 2, 0, 2] } else { &[5, 1, 0] };
    stream.write_all(greeting).await?;
    let mut reply = [0u8; 2];
    stream.read_exact(&mut reply).await?;
    match (reply[1], credentials) {
        (0, _) => {}
        (2, Some((user, pass))) => {
            let mut auth = vec![1, u8::try_from(user.len()).unwrap_or(255)];
            auth.extend_from_slice(user.as_bytes());
            auth.push(u8::try_from(pass.len()).unwrap_or(255));
            auth.extend_from_slice(pass.as_bytes());
            stream.write_all(&auth).await?;
            stream.read_exact(&mut reply).await?;
            if reply[1] != 0 {
                return Ok(None);
            }
        }
        _ => return Ok(None),
    }
    let mut connect = vec![5, 1, 0, 3, u8::try_from(host.len()).unwrap_or(0)];
    connect.extend_from_slice(host.as_bytes());
    connect.extend_from_slice(&port.to_be_bytes());
    stream.write_all(&connect).await?;
    let mut head = [0u8; 4];
    stream.read_exact(&mut head).await?;
    if head[0] != 5 || head[1] != 0 {
        return Ok(None);
    }
    // The rest of the reply: the bound address and port.
    let skip = match head[3] {
        1 => 4 + 2,
        4 => 16 + 2,
        3 => {
            let mut len = [0u8; 1];
            stream.read_exact(&mut len).await?;
            usize::from(len[0]) + 2
        }
        _ => return Ok(None),
    };
    let mut rest = vec![0u8; skip];
    stream.read_exact(&mut rest).await?;
    Ok(Some(stream))
}

async fn port_25_open() -> bool {
    let attempt = tokio::net::TcpStream::connect(("gmail-smtp-in.l.google.com", 25));
    matches!(
        tokio::time::timeout(Duration::from_secs(6), attempt).await,
        Ok(Ok(_))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_follow_outscraper() {
        let base = EmailCheck {
            valid_syntax: true,
            accepts_mail: true,
            smtp_checked: true,
            can_connect_smtp: true,
            is_deliverable: true,
            ..EmailCheck::default()
        };
        assert_eq!(finish(base.clone(), Reach::Safe).status_details, "SMTP validated");
        let role = EmailCheck { is_role: true, ..base.clone() };
        assert_eq!(finish(role, Reach::Risky).status_details, "Role");
        let catch_all = EmailCheck { is_catch_all: true, ..base.clone() };
        assert_eq!(finish(catch_all, Reach::Risky).status_details, "Catch-all");
        let no_mx = EmailCheck { accepts_mail: false, ..base.clone() };
        let no_mx = finish(no_mx, Reach::Invalid);
        assert_eq!((no_mx.status.as_str(), no_mx.status_details.as_str()), ("INVALID", "Invalid DNS"));
        let missing = EmailCheck { is_deliverable: false, ..base };
        assert_eq!(finish(missing, Reach::Invalid).status_details, "Mailbox not found");
        let blocked = EmailCheck {
            smtp_checked: false,
            error: Some("5.3.0 1.2.3.4 Rejected - see http://www.spamhaus.org".to_owned()),
            ..EmailCheck::default()
        };
        let blocked = EmailCheck { valid_syntax: true, accepts_mail: true, ..blocked };
        assert_eq!(finish(blocked, Reach::Unknown).status, "UNKNOWN");
    }

    #[test]
    fn second_look_reads_the_real_reply() {
        let probe = |real: &str, random: &str| Probe { real: real.to_owned(), random: random.to_owned() }.verdict();
        let ok = "250 2.1.5 Recipient OK";
        let ms_missing = "550 5.4.1 Recipient address rejected: Access denied. For more information see https://aka.ms/EXOSmtpErrors";
        let spamhaus = "550 5.7.1 Service unavailable, Client host [1.2.3.4] blocked using Spamhaus.";
        assert_eq!(probe(ok, ms_missing), ProbeVerdict::Accepted { catch_all: false });
        assert_eq!(probe(ok, ok), ProbeVerdict::Accepted { catch_all: true });
        assert_eq!(probe(ms_missing, ms_missing), ProbeVerdict::Missing);
        assert_eq!(probe("550 5.1.1 <jo@acme.com>: Recipient address rejected: User unknown", ""), ProbeVerdict::Missing);
        assert_eq!(probe(spamhaus, spamhaus), ProbeVerdict::Blocked);
        assert_eq!(probe("451 4.7.1 Greylisted, try again later", ""), ProbeVerdict::Unclear);
        assert_eq!(probe("550 5.7.1 Message rejected", ""), ProbeVerdict::Unclear);
        let local = random_local();
        assert_eq!(local.len(), 15);
        assert!(local.chars().all(|c| c.is_ascii_alphanumeric()));
    }
}
