//! Routes, admin sessions and forms.

use crate::{
    App, alert, db, html, now,
    store::{self, Target, UploadError},
    token,
};
use axum::{
    Form, Router,
    extract::{DefaultBodyLimit, FromRequestParts, Multipart, Path, Query, State},
    http::{HeaderMap, Method, StatusCode, header, request::Parts},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use serde::Deserialize;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;

type AppState = Arc<App>;

const SESSION_COOKIE: &str = "partage_session";
const SESSION_TTL: i64 = 30 * 86400;
const MAX_FAILURES: u32 = 10;
const FAILURE_WINDOW: Duration = Duration::from_secs(15 * 60);

pub fn router(app: AppState) -> Router {
    let uploads = Router::new()
        .route("/admin/upload", post(admin_upload))
        .route("/u/{token}", get(invite_page).post(invite_upload))
        .layer(DefaultBodyLimit::disable());
    Router::new()
        .route("/", get(dashboard))
        .route("/login", get(login_page).post(login))
        .route("/logout", post(logout))
        .route("/admin/limit", post(set_limit))
        .route("/admin/test-alert", post(test_alert))
        .route("/admin/webhook", post(set_webhook))
        .route("/admin/files/{id}/delete", post(delete_file))
        .route("/admin/invites", post(create_invite))
        .route("/admin/invites/{token}/revoke", post(revoke_invite))
        .route("/f/{id}", get(download))
        .route("/health", get(|| async { "ok" }))
        .merge(uploads)
        .with_state(app)
}

/// Where the app is reached, for the links it hands out.
fn base_url(app: &App, headers: &HeaderMap) -> String {
    if let Some(url) = &app.cfg.public_url {
        return url.clone();
    }
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("localhost");
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("http");
    format!("{proto}://{host}")
}

fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|h| h.to_str().ok())
        .flat_map(|h| h.split(';'))
        .find_map(|kv| {
            let (k, v) = kv.trim().split_once('=')?;
            (k == name).then(|| v.to_string())
        })
}

/// The visitor's address, as facade's entry point reports it.
fn client_ip(headers: &HeaderMap) -> String {
    headers
        .get("x-real-ip")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("unknown")
        .to_string()
}

fn internal(e: anyhow::Error) -> Response {
    tracing::error!("{e:#}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "Something went wrong on the server.",
    )
        .into_response()
}

/// Present in a handler's arguments, it requires a logged-in admin.
pub struct Admin;

impl FromRequestParts<AppState> for Admin {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, app: &AppState) -> Result<Self, Response> {
        if let Some(t) = cookie(&parts.headers, SESSION_COOKIE)
            && app.db.session_valid(&t, now())
        {
            return Ok(Admin);
        }
        Err(if parts.method == Method::GET {
            Redirect::to("/login").into_response()
        } else {
            (
                StatusCode::UNAUTHORIZED,
                "You are logged out: reload the page and log in again.",
            )
                .into_response()
        })
    }
}

async fn login_page() -> Html<String> {
    Html(html::login(None))
}

#[derive(Deserialize)]
struct LoginForm {
    password: String,
}

async fn login(
    State(app): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<LoginForm>,
) -> Response {
    let ip = client_ip(&headers);
    {
        let mut failures = app.login_failures.lock().unwrap();
        failures.retain(|_, (_, since)| since.elapsed() < FAILURE_WINDOW);
        if failures.get(&ip).is_some_and(|(n, _)| *n >= MAX_FAILURES) {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                Html(html::login(Some(
                    "Too many attempts: try again in 15 minutes.",
                ))),
            )
                .into_response();
        }
    }
    let right: bool = form
        .password
        .as_bytes()
        .ct_eq(app.cfg.password.as_bytes())
        .into();
    if !right {
        app.login_failures
            .lock()
            .unwrap()
            .entry(ip.clone())
            .or_insert((0, Instant::now()))
            .0 += 1;
        tracing::warn!("failed login from {ip}");
        tokio::time::sleep(Duration::from_millis(500)).await;
        return (
            StatusCode::UNAUTHORIZED,
            Html(html::login(Some("Wrong password."))),
        )
            .into_response();
    }
    app.login_failures.lock().unwrap().remove(&ip);
    let session = token(32);
    if let Err(e) = app.db.create_session(&session, now() + SESSION_TTL) {
        return internal(e);
    }
    let secure = if base_url(&app, &headers).starts_with("https://") {
        "; Secure"
    } else {
        ""
    };
    let cookie = format!(
        "{SESSION_COOKIE}={session}; Path=/; HttpOnly; SameSite=Lax; Max-Age={SESSION_TTL}{secure}"
    );
    ([(header::SET_COOKIE, cookie)], Redirect::to("/")).into_response()
}

async fn logout(State(app): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(t) = cookie(&headers, SESSION_COOKIE) {
        let _ = app.db.delete_session(&t);
    }
    let clear = format!("{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0");
    ([(header::SET_COOKIE, clear)], Redirect::to("/login")).into_response()
}

#[derive(Deserialize)]
struct Flash {
    msg: Option<String>,
}

/// Messages a redirect back to the dashboard can show.
fn flash_text(key: &str) -> Option<&'static str> {
    Some(match key {
        "limit" => "Storage limit updated.",
        "alert-sent" => "Test alert sent to Discord.",
        "alert-failed" => "The test alert could not be sent: see the logs.",
        "no-webhook" => "No Discord webhook set.",
        "webhook" => "Discord webhook saved: send a test alert to check it.",
        "webhook-removed" => "Discord webhook removed: no alerts will be sent.",
        "bad-webhook" => "That is not a Discord webhook URL (https://discord.com/api/webhooks/…).",
        "deleted" => "File deleted.",
        "invite" => "Upload link created: copy it from the list below.",
        "revoked" => "Upload link revoked.",
        "bad-size" => "That size is not valid.",
        _ => return None,
    })
}

fn back(msg: &str) -> Response {
    Redirect::to(&format!("/?msg={msg}")).into_response()
}

async fn dashboard(
    _: Admin,
    State(app): State<AppState>,
    headers: HeaderMap,
    Query(flash): Query<Flash>,
) -> Response {
    let (files, invites) = match (app.db.live_files(), app.db.recent_invites()) {
        (Ok(f), Ok(i)) => (f, i),
        (Err(e), _) | (_, Err(e)) => return internal(e),
    };
    let base = base_url(&app, &headers);
    Html(html::dashboard(&html::Dashboard {
        base: &base,
        flash: flash.msg.as_deref().and_then(flash_text),
        used: app.used(),
        limit: app.limit(),
        webhook: app.webhook().is_some(),
        files: &files,
        invites: &invites,
    }))
    .into_response()
}

fn bytes(amount: f64, unit: &str) -> Option<u64> {
    let scale = match unit {
        "MB" => 1e6,
        "GB" => 1e9,
        "TB" => 1e12,
        _ => return None,
    };
    let n = amount * scale;
    (n.is_finite() && n >= 1e6).then_some(n as u64)
}

#[derive(Deserialize)]
struct LimitForm {
    amount: f64,
    unit: String,
}

async fn set_limit(_: Admin, State(app): State<AppState>, Form(form): Form<LimitForm>) -> Response {
    let Some(limit) = bytes(form.amount, &form.unit) else {
        return back("bad-size");
    };
    if let Err(e) = app.db.set_setting("storage_limit", &limit.to_string()) {
        return internal(e);
    }
    app.limit.store(limit, std::sync::atomic::Ordering::Relaxed);
    tracing::info!("storage limit set to {}", html::fmt_bytes(limit));
    alert::check_usage(&app).await;
    back("limit")
}

#[derive(Deserialize)]
struct WebhookForm {
    url: String,
}

/// Saves the Discord webhook, or removes it when the URL is empty.
async fn set_webhook(
    _: Admin,
    State(app): State<AppState>,
    Form(form): Form<WebhookForm>,
) -> Response {
    let url = form.url.trim();
    let discord = [
        "discord.com",
        "discordapp.com",
        "ptb.discord.com",
        "canary.discord.com",
    ]
    .iter()
    .any(|host| url.starts_with(&format!("https://{host}/api/webhooks/")));
    if !url.is_empty() && !discord {
        return back("bad-webhook");
    }
    let saved = if url.is_empty() {
        app.db.delete_setting("discord_webhook")
    } else {
        app.db.set_setting("discord_webhook", url)
    };
    if let Err(e) = saved {
        return internal(e);
    }
    *app.webhook.write().unwrap() = (!url.is_empty()).then(|| url.to_string());
    tracing::info!(
        "discord webhook {}",
        if url.is_empty() { "removed" } else { "set" }
    );
    back(if url.is_empty() {
        "webhook-removed"
    } else {
        "webhook"
    })
}

async fn test_alert(_: Admin, State(app): State<AppState>) -> Response {
    if app.webhook().is_none() {
        return back("no-webhook");
    }
    let msg = format!(
        "@everyone ✅ Test alert from partage: storage at {} of {}.",
        html::fmt_bytes(app.used()),
        html::fmt_bytes(app.limit())
    );
    match alert::send(&app, msg).await {
        Ok(()) => back("alert-sent"),
        Err(e) => {
            tracing::error!("test alert: {e:#}");
            back("alert-failed")
        }
    }
}

async fn delete_file(_: Admin, State(app): State<AppState>, Path(id): Path<String>) -> Response {
    store::remove(&app, &id).await;
    alert::check_usage(&app).await;
    back("deleted")
}

async fn admin_upload(
    _: Admin,
    State(app): State<AppState>,
    headers: HeaderMap,
    form: Multipart,
) -> Result<Response, UploadError> {
    let target = Target {
        expires_at: None,
        expiry_from_form: true,
        max_bytes: None,
        invite: None,
    };
    let f = store::receive(&app, form, target).await?;
    Ok(store::upload_response(&f, &base_url(&app, &headers)))
}

#[derive(Deserialize)]
struct InviteForm {
    note: String,
    amount: f64,
    unit: String,
    valid_for: f64,
    valid_unit: String,
    file_days: String,
}

async fn create_invite(
    _: Admin,
    State(app): State<AppState>,
    Form(form): Form<InviteForm>,
) -> Response {
    let Some(max_bytes) = bytes(form.amount, &form.unit) else {
        return back("bad-size");
    };
    let valid = form.valid_for
        * match form.valid_unit.as_str() {
            "hours" => 3600.0,
            _ => 86400.0,
        };
    let file_ttl = match form.file_days.trim() {
        "" => None,
        days => match days.parse::<f64>() {
            Ok(d) if d > 0.0 && d.is_finite() => Some((d * 86400.0) as i64),
            _ => return back("bad-size"),
        },
    };
    if !(valid.is_finite() && valid > 0.0) {
        return back("bad-size");
    }
    let now = now();
    let invite = db::Invite {
        token: token(24),
        note: form.note.trim().chars().take(200).collect(),
        max_bytes,
        created_at: now,
        expires_at: now.saturating_add(valid as i64),
        file_ttl,
        claimed: false,
        used_at: None,
        revoked: false,
        file_id: None,
    };
    if let Err(e) = app.db.insert_invite(&invite) {
        return internal(e);
    }
    back("invite")
}

async fn revoke_invite(
    _: Admin,
    State(app): State<AppState>,
    Path(token): Path<String>,
) -> Response {
    if let Err(e) = app.db.revoke_invite(&token) {
        return internal(e);
    }
    back("revoked")
}

fn invite_unusable() -> Response {
    html::message(
        StatusCode::GONE,
        "Link not usable",
        "This upload link has already been used, has expired, or was revoked.",
    )
}

async fn invite_page(State(app): State<AppState>, Path(token): Path<String>) -> Response {
    match app.db.invite(&token) {
        Ok(Some(i)) if i.used_at.is_none() && !i.revoked && i.expires_at > now() => {
            Html(html::invite(&i)).into_response()
        }
        Ok(_) => invite_unusable(),
        Err(e) => internal(e),
    }
}

async fn invite_upload(
    State(app): State<AppState>,
    headers: HeaderMap,
    Path(token): Path<String>,
    form: Multipart,
) -> Response {
    let invite = match app.db.claim_invite(&token, now()) {
        Ok(true) => match app.db.invite(&token) {
            Ok(Some(i)) => i,
            Ok(None) => return invite_unusable(),
            Err(e) => return internal(e),
        },
        Ok(false) => {
            return (
                StatusCode::GONE,
                "This upload link has already been used, has expired, or is in use right now.",
            )
                .into_response();
        }
        Err(e) => return internal(e),
    };
    let target = Target {
        expires_at: invite.file_ttl.map(|ttl| now().saturating_add(ttl)),
        expiry_from_form: false,
        max_bytes: Some(invite.max_bytes),
        invite: Some(token.clone()),
    };
    match store::receive(&app, form, target).await {
        Ok(f) => {
            if let Err(e) = app.db.complete_invite(&token, &f.id, now()) {
                return internal(e);
            }
            tracing::info!("upload link {token} used for {}", f.name);
            store::upload_response(&f, &base_url(&app, &headers))
        }
        Err(e) => {
            if let Err(e) = app.db.release_invite(&token) {
                tracing::error!("releasing upload link: {e:#}");
            }
            e.into_response()
        }
    }
}

async fn download(State(app): State<AppState>, method: Method, Path(id): Path<String>) -> Response {
    store::download(&app, &id, method == Method::HEAD).await
}
