//! Lead enrichment: emails, website details and Google's place details,
//! all found deterministically (no AI).

pub mod emails;
pub mod place;
pub mod run;
pub mod site;
#[cfg(feature = "verify")]
pub mod verify;

#[cfg(test)]
mod live {
    /// `EMAILS_FILE=… cargo test --features verify verify_timing -- --ignored --nocapture`
    #[cfg(feature = "verify")]
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "network"]
    async fn verify_timing() {
        use futures::StreamExt;
        check_if_email_exists::initialize_crypto_provider();
        let file = std::env::var("EMAILS_FILE").expect("EMAILS_FILE");
        let emails: Vec<String> = std::fs::read_to_string(file).unwrap().lines().map(str::to_owned).collect();
        let verifier = std::sync::Arc::new(super::verify::Verifier::new(&[], true).await);
        let started = std::time::Instant::now();
        let mut times: Vec<(f64, String, String)> = futures::stream::iter(emails)
            .map(|email| {
                let verifier = verifier.clone();
                async move {
                    let t = std::time::Instant::now();
                    let check = verifier.check(&email).await;
                    (t.elapsed().as_secs_f64(), email, format!("{} / {}", check.reachable, check.status_details))
                }
            })
            .buffer_unordered(30)
            .collect()
            .await;
        times.sort_by(|a, b| b.0.total_cmp(&a.0));
        for (secs, email, verdict) in &times {
            println!("{secs:6.1}s {email} {verdict}");
        }
        let total: f64 = times.iter().map(|t| t.0).sum();
        println!("emails={} wall={:.1}s mean={:.1}s", times.len(), started.elapsed().as_secs_f64(), total / times.len() as f64);
    }

    /// `SITES_FILE=… cargo test site_timing -- --ignored --nocapture`:
    /// how long reading each site takes, `AT_ONCE` at once (30 by default).
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "network"]
    async fn site_timing() {
        use futures::StreamExt;
        let file = std::env::var("SITES_FILE").expect("SITES_FILE");
        let sites: Vec<String> = std::fs::read_to_string(file).unwrap().lines().map(str::to_owned).collect();
        let reader = std::sync::Arc::new(super::site::SiteReader::new().expect("reader"));
        let at_once: usize = std::env::var("AT_ONCE").ok().and_then(|value| value.parse().ok()).unwrap_or(30);
        let started = std::time::Instant::now();
        let mut times: Vec<(f64, usize, usize, String, Option<String>)> = futures::stream::iter(sites)
            .map(|site| {
                let reader = reader.clone();
                async move {
                    let t = std::time::Instant::now();
                    let report = reader.read(&site).await;
                    (t.elapsed().as_secs_f64(), report.pages.len(), report.emails.len(), site, report.error)
                }
            })
            .buffer_unordered(at_once)
            .collect()
            .await;
        times.sort_by(|a, b| b.0.total_cmp(&a.0));
        for (secs, pages, emails, site, error) in &times {
            println!("{secs:6.1}s pages={pages} emails={emails} {site} {}", error.as_deref().unwrap_or(""));
        }
        let total: f64 = times.iter().map(|t| t.0).sum();
        println!("sites={} wall={:.1}s mean={:.1}s", times.len(), started.elapsed().as_secs_f64(), total / times.len() as f64);
    }

    /// `cargo test live_sites -- --ignored --nocapture`: reads real sites.
    #[tokio::test]
    #[ignore = "network"]
    async fn live_sites() {
        let reader = super::site::SiteReader::new().expect("reader");
        let sites = std::env::var("SITES").unwrap_or_else(|_| {
            "https://www.outdoorspacesky.com/ http://customlawncareky.com/ https://stonebluehardscape.com/ https://terracraftos.com/".to_owned()
        });
        for site in sites.split_whitespace() {
            let started = std::time::Instant::now();
            let report = reader.read(site).await;
            println!("\n== {site} ({:.1}s, {} pages) error={:?}", started.elapsed().as_secs_f64(), report.pages.len(), report.error);
            println!("pages: {:?}", report.pages);
            println!("title: {:?}\ndesc: {:?}\ngenerator: {:?} gtm={} pixel={}", report.title, report.description, report.generator, report.has_gtm, report.has_fb_pixel);
            println!("socials: {:?}\nphones: {:?}\ncontacts: {:?}", report.socials, report.phones, report.contacts);
            for email in &report.emails {
                println!("  {} [{}] on {}", email.email, email.found, email.page);
            }
        }
    }
}
