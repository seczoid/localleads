//! First-run setup, sign-in, sign-out and changing the password. One
//! account owns the app; there is no sign-up after the first, no password
//! reset and no two-factor.

use super::site::{ClientIp, clear_failures, locked_out, minutes, record_failure, site};
use super::{AppError, WebResult, notify, render};
use crate::app::SharedState;
use crate::auth::{self, SESSION_COOKIE, SESSION_DAYS};
use crate::db::Account;
use askama::Template;
use axum::extract::{Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use axum_extra::extract::CookieJar;
use axum_extra::extract::Form;
use axum_extra::headers::authorization::Bearer;
use axum_extra::headers::{Authorization, HeaderMapExt};
use serde::Deserialize;
use std::time::Duration;

#[derive(Template)]
#[template(path = "auth.html")]
struct AuthPage {
    setup: bool,
    /// Setup asks for the code printed in the server log.
    needs_code: bool,
    email: String,
    next: String,
    error: String,
}

impl AuthPage {
    fn new(setup: bool, email: String, next: String, error: String) -> Self {
        Self {
            setup,
            needs_code: setup && site().setup_code.is_some(),
            email,
            next,
            error,
        }
    }
}

#[derive(Deserialize, Default)]
pub struct NextQuery {
    #[serde(default)]
    next: String,
}

#[derive(Deserialize)]
pub struct SetupForm {
    #[serde(default)]
    code: String,
    #[serde(default)]
    email: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    confirm: String,
}

#[derive(Deserialize)]
pub struct LoginForm {
    #[serde(default)]
    email: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    next: String,
}

#[derive(Deserialize)]
pub struct PasswordForm {
    #[serde(default)]
    current: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    confirm: String,
}

/// Only paths on this app, so a crafted link can't send people elsewhere
/// after they sign in.
fn safe_next(next: &str) -> String {
    if next.starts_with('/') && !next.starts_with("//") && !next.starts_with("/\\") {
        next.to_owned()
    } else {
        "/".to_owned()
    }
}

fn page(status: StatusCode, page: AuthPage) -> WebResult {
    let mut response = render(page)?;
    *response.status_mut() = status;
    Ok(response)
}

fn session_hash(jar: &CookieJar) -> Option<String> {
    jar.get(SESSION_COOKIE)
        .map(|cookie| cookie.value())
        .filter(|value| !value.is_empty())
        .map(auth::digest)
}

async fn blocking<T: Send + 'static>(work: impl FnOnce() -> anyhow::Result<T> + Send + 'static) -> Result<T, AppError> {
    Ok(tokio::task::spawn_blocking(work)
        .await
        .map_err(anyhow::Error::from)??)
}

/// Signs `account` in and sends them to `next`.
async fn start_session(state: &SharedState, headers: &HeaderMap, account: &Account, next: &str) -> WebResult {
    let token = auth::new_session_token();
    let expires = (chrono::Utc::now() + chrono::Duration::days(SESSION_DAYS)).to_rfc3339();
    let db = state.db.clone();
    let (user, hash) = (account.id.clone(), auth::digest(&token));
    blocking(move || db.create_session(&user, &hash, &expires)).await?;
    // Over HTTPS the cookie is only ever sent over HTTPS.
    let secure = site().request_is_https(headers);
    let cookie = format!(
        "{SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}{}",
        SESSION_DAYS * 24 * 60 * 60,
        if secure { "; Secure" } else { "" }
    );
    let mut response = Redirect::to(&safe_next(next)).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie).map_err(anyhow::Error::from)?,
    );
    Ok(response)
}

async fn has_account(state: &SharedState) -> Result<bool, AppError> {
    let db = state.db.clone();
    blocking(move || db.has_account()).await
}

async fn signed_in(state: &SharedState, jar: &CookieJar) -> Result<Option<Account>, AppError> {
    let Some(hash) = session_hash(jar) else {
        return Ok(None);
    };
    let db = state.db.clone();
    blocking(move || db.session_account(&hash)).await
}

// ------------------------------------------------------------- pages

pub async fn setup_page(State(state): State<SharedState>) -> WebResult {
    if has_account(&state).await? {
        return Ok(Redirect::to("/login").into_response());
    }
    render(AuthPage::new(true, String::new(), "/".to_owned(), String::new()))
}

pub async fn setup(
    State(state): State<SharedState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    Form(form): Form<SetupForm>,
) -> WebResult {
    if has_account(&state).await? {
        return Ok(Redirect::to("/login").into_response());
    }
    let retry = |status: StatusCode, error: &str| {
        page(
            status,
            AuthPage::new(true, form.email.trim().to_owned(), "/".to_owned(), error.to_owned()),
        )
    };
    if let Some(wait) = locked_out(ip) {
        return retry(
            StatusCode::TOO_MANY_REQUESTS,
            &format!("Too many wrong setup codes. Try again in {}.", minutes(wait)),
        );
    }
    if let Some(code) = &site().setup_code
        && auth::digest(&form.code) != auth::digest(code)
    {
        record_failure(ip);
        tokio::time::sleep(Duration::from_millis(900)).await;
        return retry(
            StatusCode::UNAUTHORIZED,
            "That setup code isn't right. setup.sh printed it, and so does the server log when it starts.",
        );
    }
    let retry = |error: &str| retry(StatusCode::UNPROCESSABLE_ENTITY, error);
    let email = match auth::normalize_email(&form.email) {
        Ok(email) => email,
        Err(error) => return retry(error),
    };
    if let Err(error) = auth::check_new_password(&form.password, &form.confirm) {
        return retry(error);
    }
    let password = form.password.clone();
    let hash = blocking(move || auth::hash_password(&password)).await?;
    let db = state.db.clone();
    let address = email.clone();
    let Some(account) = blocking(move || db.create_account(&address, &hash)).await? else {
        // Someone else finished setup first.
        return Ok(Redirect::to("/login").into_response());
    };
    state.notice("ok", format!("Account created for {email}"));
    start_session(&state, &headers, &account, "/").await
}

pub async fn login_page(State(state): State<SharedState>, jar: CookieJar, Query(query): Query<NextQuery>) -> WebResult {
    if !has_account(&state).await? {
        return Ok(Redirect::to("/setup").into_response());
    }
    if signed_in(&state, &jar).await?.is_some() {
        return Ok(Redirect::to(&safe_next(&query.next)).into_response());
    }
    render(AuthPage::new(false, String::new(), safe_next(&query.next), String::new()))
}

pub async fn login(
    State(state): State<SharedState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    Form(form): Form<LoginForm>,
) -> WebResult {
    if !has_account(&state).await? {
        return Ok(Redirect::to("/setup").into_response());
    }
    if let Some(wait) = locked_out(ip) {
        return page(
            StatusCode::TOO_MANY_REQUESTS,
            AuthPage::new(
                false,
                form.email.trim().to_owned(),
                safe_next(&form.next),
                format!("Too many failed sign-ins. Try again in {}.", minutes(wait)),
            ),
        );
    }
    let email = form.email.trim().to_lowercase();
    let db = state.db.clone();
    let lookup = email.clone();
    let found = blocking(move || db.account_by_email(&lookup)).await?;
    let password = form.password.clone();
    // Check a hash even for an unknown email, so timing doesn't tell
    // whether the email is the account's.
    let (account, stored) = match found {
        Some((account, stored)) => (Some(account), stored),
        None => (None, auth::hash_password("unused").unwrap_or_default()),
    };
    let matches = blocking(move || Ok(auth::verify_password(&password, &stored))).await?;
    match account {
        Some(account) if matches => {
            clear_failures(ip);
            start_session(&state, &headers, &account, &form.next).await
        }
        _ => {
            // Slows down guessing; ten misses lock the address out a while.
            record_failure(ip);
            tokio::time::sleep(Duration::from_millis(900)).await;
            page(
                StatusCode::UNAUTHORIZED,
                AuthPage::new(
                    false,
                    form.email.trim().to_owned(),
                    safe_next(&form.next),
                    "That email and password don't match.".to_owned(),
                ),
            )
        }
    }
}

pub async fn logout(State(state): State<SharedState>, jar: CookieJar) -> WebResult {
    if let Some(hash) = session_hash(&jar) {
        let db = state.db.clone();
        blocking(move || db.delete_session(&hash)).await?;
    }
    let mut response = Redirect::to("/login").into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_static("ll_session=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0"),
    );
    Ok(response)
}

pub async fn change_password(
    State(state): State<SharedState>,
    ClientIp(ip): ClientIp,
    jar: CookieJar,
    axum::Extension(account): axum::Extension<Account>,
    Form(form): Form<PasswordForm>,
) -> WebResult {
    if let Some(wait) = locked_out(ip) {
        return Err(AppError::bad_request(format!(
            "Too many wrong passwords. Try again in {}.",
            minutes(wait)
        )));
    }
    let db = state.db.clone();
    let user = account.id.clone();
    let stored = blocking(move || db.password_hash(&user))
        .await?
        .unwrap_or_default();
    let current = form.current.clone();
    if !blocking(move || Ok(auth::verify_password(&current, &stored))).await? {
        record_failure(ip);
        tokio::time::sleep(Duration::from_millis(900)).await;
        return Err(AppError::bad_request("Your current password isn't right."));
    }
    auth::check_new_password(&form.password, &form.confirm).map_err(AppError::bad_request)?;
    if form.password == form.current {
        return Err(AppError::bad_request("Pick a password different from the current one."));
    }
    let password = form.password.clone();
    let hash = blocking(move || auth::hash_password(&password)).await?;
    let keep = session_hash(&jar).unwrap_or_default();
    let db = state.db.clone();
    blocking(move || db.set_password(&account.id, &hash, &keep)).await?;
    Ok(notify(
        "ok",
        "Password changed. Other devices were signed out.",
        &["password-changed"],
    ))
}

// -------------------------------------------------------- the gate

/// Lets a request through when it carries a valid session (or, for the
/// JSON API, an API key). Otherwise sends people to sign in, or to create
/// the account when there is none yet.
pub async fn require_sign_in(State(state): State<SharedState>, jar: CookieJar, mut request: Request, next: Next) -> Response {
    let path = request.uri().path().to_owned();
    let target = request
        .uri()
        .path_and_query()
        .map_or_else(|| "/".to_owned(), |value| value.as_str().to_owned());
    let get = request.method() == axum::http::Method::GET;
    let key = path.starts_with("/api/").then(|| api_key(request.headers())).flatten();
    match gate(&state, &jar, key, &target, get).await {
        Ok(Gate::Allow(account)) => {
            if let Some(account) = account {
                request.extensions_mut().insert(account);
            }
            next.run(request).await
        }
        Ok(Gate::SignIn(to)) => {
            let htmx = request.headers().contains_key("hx-request");
            if htmx {
                // htmx follows this with a full page load.
                let mut response = StatusCode::UNAUTHORIZED.into_response();
                if let Ok(value) = HeaderValue::from_str(&to) {
                    response.headers_mut().insert("HX-Redirect", value);
                }
                response
            } else if get && !path.starts_with("/api/") && path != "/events" {
                Redirect::to(&to).into_response()
            } else {
                (StatusCode::UNAUTHORIZED, "Sign in first").into_response()
            }
        }
        Err(error) => error.into_response(),
    }
}

enum Gate {
    Allow(Option<Account>),
    SignIn(String),
}

async fn gate(state: &SharedState, jar: &CookieJar, key: Option<String>, target: &str, get: bool) -> Result<Gate, AppError> {
    if !has_account(state).await? {
        return Ok(Gate::SignIn("/setup".to_owned()));
    }
    if let Some(account) = signed_in(state, jar).await? {
        return Ok(Gate::Allow(Some(account)));
    }
    // The JSON API also takes an API key, for scripts.
    if let Some(key) = key {
        let db = state.db.clone();
        let hash = auth::digest(&key);
        if blocking(move || db.use_api_key(&hash)).await? {
            return Ok(Gate::Allow(None));
        }
    }
    let to = if target == "/" || !get {
        "/login".to_owned()
    } else {
        format!(
            "/login?next={}",
            url::form_urlencoded::byte_serialize(target.as_bytes()).collect::<String>()
        )
    };
    Ok(Gate::SignIn(to))
}

/// An API key from `Authorization: Bearer …`, or `X-API-Key` for clients
/// that can only set a plain header.
pub fn api_key(headers: &HeaderMap) -> Option<String> {
    headers
        .typed_get::<Authorization<Bearer>>()
        .map(|bearer| bearer.token().trim().to_owned())
        .or_else(|| {
            headers
                .get("x-api-key")
                .and_then(|value| value.to_str().ok())
                .map(|value| value.trim().to_owned())
        })
        .filter(|key| !key.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_stays_on_this_app() {
        assert_eq!(safe_next("/leads?x=1"), "/leads?x=1");
        assert_eq!(safe_next("//evil.example"), "/");
        assert_eq!(safe_next("https://evil.example"), "/");
        assert_eq!(safe_next("/\\\\evil.example"), "/");
        assert_eq!(safe_next(""), "/");
    }

    #[test]
    fn api_keys_come_from_either_header() {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer ll_abc"));
        assert_eq!(api_key(&headers).as_deref(), Some("ll_abc"));
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static(" ll_def "));
        assert_eq!(api_key(&headers).as_deref(), Some("ll_def"));
        assert_eq!(api_key(&HeaderMap::new()), None);
    }
}
