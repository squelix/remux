# Trakt Dashboard UI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let an admin configure Trakt app credentials and let any logged-in user connect/disconnect their personal Trakt account, entirely from the `remux-dashboard` web UI — no curl required.

**Architecture:** Four new typed `Endpoint` structs + three response DTOs added to `remux-sdks` (the shared client/server contract layer), an admin-only `TraktSettingsCard` under the existing Settings section, and a new self-service `TraktAccountCard` under a new "Integrations" sidebar group visible to every logged-in user. A small backend fix makes the "admin hasn't configured Trakt" error detectable by the frontend.

**Tech Stack:** Rust, Dioxus (WASM dashboard), `remux-sdks` typed `RestClient`/`Endpoint` pattern, `gloo_timers` for polling, `web_sys` for opening the Trakt verification URL in a new tab.

## Global Constraints

- Dashboard components fetch data via `client.execute(SomeEndpoint).await -> Result<Output, ClientError>`, never raw `fetch` — `AppState.client: RestClient<JellyfinAuth>` (`crates/remux-dashboard/src/state.rs:33`).
- Errors are surfaced via `e.user_message()` into the existing `ErrorAlert` component — never a raw `{e}` debug format in user-facing text (matches every existing settings card).
- No automated tests exist for Dioxus components in this repo (verified: zero `#[test]` in `crates/remux-dashboard/src`) — this plan does not introduce an exception. `remux-sdks` `Endpoint` structs also have no colocated tests in the existing `remux`-namespace endpoints (`GetSystemConfiguration`, `GetScheduledTasks`, etc.) — this plan follows that precedent and does not add tests for the new endpoint structs either.
- Verification for dashboard changes is `cargo check -p remux-dashboard` (confirmed to type-check cleanly on the native target without needing a wasm target or `dx build`).
- `POST /system/configuration` replaces the entire `ServerConfiguration` — any save flow must GET-then-merge-then-POST, never construct a fresh `ServerConfiguration::default()` and post that.
- Route naming convention: `<Group><SubPage>Route` mapping to `/<group>/<sub-page>` (e.g. `SettingsJellyfinSyncRoute` → `/settings/jellyfin-sync`).

---

### Task 1: SDK — Trakt auth endpoints, response DTOs, and a backend error-detail fix

**Files:**
- Modify: `crates/remux-sdks/src/remux/mod.rs` (add 3 response DTOs + 4 `Endpoint` structs)
- Modify: `crates/remux-server/src/api/trakt.rs` (remove local DTO defs, import from `remux_sdks::remux`, fix two handlers to surface the real error message)

**Interfaces:**
- Consumes: `crate::{Auth, Body, Endpoint, RestClient}` and `Method`/`Deserialize`/`Serialize` (already imported at the top of `remux-sdks/src/remux/mod.rs`); `crate::trakt::PollStatus` and `db::TraktToken`/`db::Settings` (existing, `remux-server`).
- Produces: `remux_sdks::remux::{TraktAuthStartResponse, TraktAuthPollResponse, TraktAuthStatusResponse, StartTraktAuth, PollTraktAuth, GetTraktAuthStatus, DisconnectTraktAuth}` — consumed by Task 2 (none) and Task 3 (`GetTraktAuthStatus`, `StartTraktAuth`, `PollTraktAuth`, `DisconnectTraktAuth`, and the three response types' exact field names: `user_code: String`, `verification_url: String`, `interval: i64`, `expires_in: i64` on `TraktAuthStartResponse`; `status: String` on `TraktAuthPollResponse`; `connected: bool` on `TraktAuthStatusResponse`).

- [ ] **Step 1: Add the response DTOs and endpoints to `remux-sdks`**

Append to `crates/remux-sdks/src/remux/mod.rs` (anywhere after the existing `use` block near the top — e.g. right after the `GetSystemConfiguration`/`UpdateSystemConfiguration` definitions around line 4872, to stay near the other `/system/configuration`-adjacent endpoints):

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct TraktAuthStartResponse {
    pub user_code: String,
    pub verification_url: String,
    pub interval: i64,
    pub expires_in: i64,
}

#[derive(Debug, Clone, Default)]
pub struct StartTraktAuth;

impl Endpoint for StartTraktAuth {
    type Output = TraktAuthStartResponse;
    fn path(&self) -> String {
        "/remux/trakt/auth/start".into()
    }
    fn method(&self) -> Method {
        Method::POST
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct TraktAuthPollResponse {
    pub status: String,
}

#[derive(Debug, Clone, Default)]
pub struct PollTraktAuth;

impl Endpoint for PollTraktAuth {
    type Output = TraktAuthPollResponse;
    fn path(&self) -> String {
        "/remux/trakt/auth/poll".into()
    }
    fn method(&self) -> Method {
        Method::POST
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct TraktAuthStatusResponse {
    pub connected: bool,
}

#[derive(Debug, Clone, Default)]
pub struct GetTraktAuthStatus;

impl Endpoint for GetTraktAuthStatus {
    type Output = TraktAuthStatusResponse;
    fn path(&self) -> String {
        "/remux/trakt/auth/status".into()
    }
}

#[derive(Debug, Clone, Default)]
pub struct DisconnectTraktAuth;

impl Endpoint for DisconnectTraktAuth {
    type Output = ();
    fn path(&self) -> String {
        "/remux/trakt/auth".into()
    }
    fn method(&self) -> Method {
        Method::DELETE
    }
}
```

- [ ] **Step 2: Update `remux-server`'s `api/trakt.rs` to consume the relocated DTOs**

Replace the full contents of `crates/remux-server/src/api/trakt.rs` with:

```rust
use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use remux_macros::{delete, get, post};

use crate::{AppState, ResultExt, db, db::auth, sdks, trakt::PollStatus};
use axum_anyhow::ApiResult as Result;
use remux_sdks::remux::{
    TraktAuthPollResponse, TraktAuthStartResponse, TraktAuthStatusResponse,
};

#[post("/remux/trakt/auth/start")]
pub async fn start_trakt_auth(
    State(state): State<AppState>,
    session: auth::AuthSession,
) -> Result<impl IntoResponse> {
    let resp = state
        .ctx
        .trakt_auth
        .start(
            &state.ctx.db,
            &state.ctx.config.trakt_base_url,
            session.user.id,
        )
        .await
        .map_err(|e| {
            let detail = e.to_string();
            e.context_internal(&detail)
        })?;

    Ok(Json(TraktAuthStartResponse {
        user_code: resp.user_code,
        verification_url: resp.verification_url,
        interval: resp.interval,
        expires_in: resp.expires_in,
    }))
}

#[post("/remux/trakt/auth/poll")]
pub async fn poll_trakt_auth(
    State(state): State<AppState>,
    session: auth::AuthSession,
) -> Result<impl IntoResponse> {
    let status = state
        .ctx
        .trakt_auth
        .poll(
            &state.ctx.db,
            &state.ctx.config.trakt_base_url,
            session.user.id,
        )
        .await
        .map_err(|e| {
            let detail = e.to_string();
            e.context_internal(&detail)
        })?;

    let status = match status {
        PollStatus::Pending => "pending",
        PollStatus::Authorized => "authorized",
        PollStatus::Expired => "expired",
        PollStatus::Denied => "denied",
    };

    Ok(Json(TraktAuthPollResponse {
        status: status.to_string(),
    }))
}

#[get("/remux/trakt/auth/status")]
pub async fn trakt_auth_status(
    State(state): State<AppState>,
    session: auth::AuthSession,
) -> Result<impl IntoResponse> {
    let connected = db::TraktToken::get_by_user(&state.ctx.db, session.user.id)
        .await
        .context_internal("failed to load Trakt token")?
        .is_some();

    Ok(Json(TraktAuthStatusResponse { connected }))
}

#[delete("/remux/trakt/auth")]
pub async fn disconnect_trakt_auth(
    State(state): State<AppState>,
    session: auth::AuthSession,
) -> Result<impl IntoResponse> {
    state
        .ctx
        .trakt_auth
        .cancel(session.user.id);

    // Best-effort revoke on Trakt's side — a failure here (network, already
    // revoked, missing config) must not block deleting the local token.
    if let Ok(Some(token)) = db::TraktToken::get_by_user(&state.ctx.db, session.user.id).await {
        let cfg = db::Settings::get_config(&state.ctx.db)
            .await
            .unwrap_or_default();
        if let (Some(client_id), Some(client_secret)) =
            (cfg.trakt_client_id, cfg.trakt_client_secret)
        {
            if let Ok(client) = sdks::RestClient::new(&state.ctx.config.trakt_base_url)
                .map(|c| c.with_auth(sdks::trakt::TraktOAuthAuth))
            {
                let _ = client
                    .execute(sdks::trakt::RevokeTokenEndpoint {
                        client_id,
                        client_secret,
                        token: token.access_token,
                    })
                    .await;
            }
        }
    }

    db::TraktToken::delete(&state.ctx.db, session.user.id)
        .await
        .context_internal("failed to delete Trakt token")?;
    Ok(StatusCode::NO_CONTENT.into_response())
}
```

**Why the `map_err` change matters:** `ResultExt::context_internal(self, detail: &str)` (`crates/remux-server/src/result_ext.rs`) always sends the literal `detail` string to the client as the HTTP error response's `detail` field — the underlying error's actual message (e.g. "Trakt client_id is not configured", raised by `TraktAuthService::start`/`poll` when the admin hasn't configured credentials) is stored server-side but never serialized to the client. The old code passed a fixed generic string ("failed to start Trakt device authorization"), making the specific "not configured" case indistinguishable from any other failure once it reaches the browser. The fix above forwards the real `anyhow::Error`'s message as the detail, so Task 3's UI can reliably detect this case by matching on the response text via `ClientError::user_message()`.

- [ ] **Step 3: Verify it builds**

Run: `cargo build -p remux-sdks -p remux-server`
Expected: builds cleanly (0 errors).

- [ ] **Step 4: Run the existing Trakt-related test suite to confirm no regressions**

Run: `cargo test -p remux-sdks -p remux-server -- --test-threads=4`
Expected: PASS, same count as before this change (this task adds no new tests, per the Global Constraints note — the existing `trakt` test suites from the backend feature must still pass unchanged).

- [ ] **Step 5: Commit**

```bash
git add crates/remux-sdks/src/remux/mod.rs crates/remux-server/src/api/trakt.rs
git commit -m "feat(trakt-ui): add SDK endpoints for Trakt auth and surface real error details"
```

---

### Task 2: Admin Settings — `TraktSettingsCard`

**Files:**
- Modify: `crates/remux-dashboard/src/pages/settings.rs` (add `TraktSettingsCard`)
- Modify: `crates/remux-dashboard/src/pages/mod.rs` (export it)
- Modify: `crates/remux-dashboard/src/router.rs` (new `SettingsTraktRoute` + wrapper)
- Modify: `crates/remux-dashboard/src/layout.rs` (nav entry, page title, active-route check)

**Interfaces:**
- Consumes: `remux_sdks::remux::{GetSystemConfiguration, UpdateSystemConfiguration, ServerConfiguration}` (existing, already imported at the top of `pages/settings.rs`), `ServerConfiguration.trakt_client_id`/`trakt_client_secret: Option<String>` (existing fields from the backend feature), `crate::components::{Card, ErrorAlert, LoadingText, SuccessAlert}` (existing), `crate::state::AppState` (existing).
- Produces: `pages::settings::TraktSettingsCard` component, re-exported as `pages::TraktSettingsCard` — consumed only by `router.rs`'s new route wrapper in this task (no other task depends on it).

- [ ] **Step 1: Add `TraktSettingsCard` to `pages/settings.rs`**

Append this component to `crates/remux-dashboard/src/pages/settings.rs` (no new imports needed — `Card`, `ErrorAlert`, `LoadingText`, `SuccessAlert`, `ServerConfiguration`, `GetSystemConfiguration`, `UpdateSystemConfiguration` are already imported at the top of this file):

```rust
#[component]
pub fn TraktSettingsCard(app_state: AppState) -> Element {
    let mut base_cfg: Signal<Option<ServerConfiguration>> = use_signal(|| None);
    let mut client_id = use_signal(String::new);
    let mut client_secret = use_signal(String::new);
    let mut loading = use_signal(|| true);
    let mut saving = use_signal(|| false);
    let mut save_error = use_signal(|| Option::<String>::None);
    let mut saved = use_signal(|| false);

    let app_state_load = app_state.clone();
    use_effect(move || {
        let client = app_state_load
            .client
            .clone();
        spawn(async move {
            match client
                .execute(GetSystemConfiguration)
                .await
            {
                Ok(cfg) => {
                    client_id.set(
                        cfg.trakt_client_id
                            .clone()
                            .unwrap_or_default(),
                    );
                    client_secret.set(
                        cfg.trakt_client_secret
                            .clone()
                            .unwrap_or_default(),
                    );
                    base_cfg.set(Some(cfg));
                }
                Err(e) => save_error.set(Some(format!("Failed to load settings: {e}"))),
            }
            loading.set(false);
        });
    });

    let on_save = move |e: Event<FormData>| {
        e.prevent_default();
        let client = app_state
            .client
            .clone();
        let id = client_id
            .peek()
            .clone();
        let secret = client_secret
            .peek()
            .clone();

        let mut cfg = base_cfg
            .peek()
            .clone()
            .unwrap_or_default();
        cfg.trakt_client_id = if id.is_empty() { None } else { Some(id) };
        cfg.trakt_client_secret = if secret.is_empty() { None } else { Some(secret) };

        saving.set(true);
        save_error.set(None);
        saved.set(false);
        spawn(async move {
            match client
                .execute(UpdateSystemConfiguration { config: cfg })
                .await
            {
                Ok(_) => saved.set(true),
                Err(e) => save_error.set(Some(e.user_message())),
            }
            saving.set(false);
        });
    };

    rsx! {
        Card { title: "Trakt App Credentials",
            if *loading.read() {
                LoadingText {}
            } else {
                form {
                    onsubmit: on_save,
                    style: "display:flex;flex-direction:column;gap:14px",

                    div { class: "field",
                        label { class: "field-label", r#for: "trakt-client-id", "Client ID" }
                        input {
                            id: "trakt-client-id",
                            r#type: "text",
                            class: "field-input",
                            placeholder: "Trakt app client ID",
                            value: "{client_id}",
                            oninput: move |e| client_id.set(e.value()),
                        }
                        p { class: "field-hint",
                            "Register an app at trakt.tv/oauth/applications to get these."
                        }
                    }

                    div { class: "field",
                        label { class: "field-label", r#for: "trakt-client-secret", "Client Secret" }
                        input {
                            id: "trakt-client-secret",
                            r#type: "password",
                            class: "field-input",
                            placeholder: "••••••••••••••••",
                            value: "{client_secret}",
                            oninput: move |e| client_secret.set(e.value()),
                        }
                    }

                    if let Some(err) = save_error.read().as_ref() {
                        ErrorAlert { message: err.clone() }
                    }
                    if *saved.read() {
                        SuccessAlert { message: "Settings saved.".to_string() }
                    }

                    div { class: "form-actions", style: "display:flex;gap:8px;align-items:center",
                        button {
                            r#type: "submit",
                            class: "btn btn-primary",
                            disabled: *saving.read(),
                            if *saving.read() { "Saving…" } else { "Save" }
                        }
                    }
                }
            }
        }
    }
}
```

- [ ] **Step 2: Export it**

In `crates/remux-dashboard/src/pages/mod.rs`, add `TraktSettingsCard` to the `pub use settings::{...}` list (alphabetically):

```rust
pub use settings::{
    IntroSettingsCard, JellyfinImportCard, P2pSettingsCard, PlaybackSettingsCard,
    ProbeSettingsCard, RemuxdbSettingsCard, SearchSettingsCard, ServerSettingsCard,
    TraktSettingsCard,
};
```

- [ ] **Step 3: Add the route**

In `crates/remux-dashboard/src/router.rs`, add a new variant to the `Route` enum (next to `SettingsJellyfinSyncRoute`):

```rust
    #[route("/settings/jellyfin-sync")]
    SettingsJellyfinSyncRoute,
    #[route("/settings/trakt")]
    SettingsTraktRoute,
```

And a matching wrapper component (next to `SettingsJellyfinSyncRoute`'s wrapper):

```rust
#[component]
pub(crate) fn SettingsTraktRoute() -> Element {
    let app_state = use_context::<AppState>();
    rsx! { TraktSettingsCard { app_state } }
}
```

- [ ] **Step 4: Add the nav entry**

In `crates/remux-dashboard/src/layout.rs`:

1. Add `Route::SettingsTraktRoute` to the Settings `SidebarGroup`'s `active` check (the `matches!` macro around line 193-201):

```rust
                        active: matches!(route,
                            Route::SettingsGeneralRoute
                            | Route::SettingsPlaybackRoute
                            | Route::SettingsSearchRoute
                            | Route::SettingsJellyfinSyncRoute
                            | Route::SettingsTraktRoute
                            | Route::SettingsBrandingRoute
                            | Route::SettingsIntroRoute
                            | Route::SettingsRemuxdbRoute
                        ),
```

2. Add a `NavSubItem` right after the "Jellyfin Sync" one (around line 217-221):

```rust
                        NavSubItem {
                            label: "Jellyfin Sync",
                            active: route == Route::SettingsJellyfinSyncRoute,
                            on_click: move |_| { navigator().push(Route::SettingsJellyfinSyncRoute); sidebar_open.set(false); },
                        }
                        NavSubItem {
                            label: "Trakt",
                            active: route == Route::SettingsTraktRoute,
                            on_click: move |_| { navigator().push(Route::SettingsTraktRoute); sidebar_open.set(false); },
                        }
```

3. Add a `page_title` match arm (in the `match route { ... }` block, near `Route::SettingsJellyfinSyncRoute => "Jellyfin Sync",`):

```rust
        Route::SettingsJellyfinSyncRoute => "Jellyfin Sync",
        Route::SettingsTraktRoute => "Trakt",
```

- [ ] **Step 5: Verify it builds**

Run: `cargo check -p remux-dashboard`
Expected: 0 errors.

- [ ] **Step 6: Commit**

```bash
git add crates/remux-dashboard/src/pages/settings.rs crates/remux-dashboard/src/pages/mod.rs crates/remux-dashboard/src/router.rs crates/remux-dashboard/src/layout.rs
git commit -m "feat(trakt-ui): add admin settings card for Trakt client_id/secret"
```

---

### Task 3: User-facing "Integrations" section — `TraktAccountCard`

**Files:**
- Create: `crates/remux-dashboard/src/pages/integrations.rs`
- Modify: `crates/remux-dashboard/src/pages/mod.rs` (register module + export)
- Modify: `crates/remux-dashboard/src/router.rs` (new `IntegrationsTraktRoute` + wrapper)
- Modify: `crates/remux-dashboard/src/layout.rs` (new `SidebarGroup { label: "Integrations" }`, page title)

**Interfaces:**
- Consumes: `remux_sdks::remux::{GetTraktAuthStatus, StartTraktAuth, PollTraktAuth, DisconnectTraktAuth, TraktAuthStartResponse, TraktAuthPollResponse}` (Task 1), `crate::components::{Card, ErrorAlert, LoadingText}` (existing), `crate::state::AppState` (existing).
- Produces: `pages::integrations::TraktAccountCard`, re-exported as `pages::TraktAccountCard` — consumed only by `router.rs`'s new route wrapper (no later task in this plan depends on it).

- [ ] **Step 1: Create `pages/integrations.rs`**

```rust
use crate::{
    components::{Card, ErrorAlert, LoadingText},
    state::AppState,
};
use dioxus::prelude::*;
use remux_sdks::remux::{DisconnectTraktAuth, GetTraktAuthStatus, PollTraktAuth, StartTraktAuth};

#[derive(Clone, PartialEq)]
enum TraktConnectionState {
    Loading,
    NotConnected,
    Connecting {
        user_code: String,
        verification_url: String,
    },
    Connected,
    Error(String),
}

#[component]
pub fn TraktAccountCard(app_state: AppState) -> Element {
    let mut state = use_signal(|| TraktConnectionState::Loading);
    let mut disconnect_error = use_signal(|| Option::<String>::None);

    let app_state_load = app_state.clone();
    use_effect(move || {
        let client = app_state_load
            .client
            .clone();
        spawn(async move {
            match client
                .execute(GetTraktAuthStatus)
                .await
            {
                Ok(resp) if resp.connected => state.set(TraktConnectionState::Connected),
                Ok(_) => state.set(TraktConnectionState::NotConnected),
                Err(e) => {
                    let msg = e.user_message();
                    if msg.contains("client_id is not configured") {
                        state.set(TraktConnectionState::Error(
                            "Trakt is not configured by the server administrator.".to_string(),
                        ));
                    } else {
                        state.set(TraktConnectionState::NotConnected);
                    }
                }
            }
        });
    });

    let app_state_connect = app_state.clone();
    let on_connect = move |_| {
        let client = app_state_connect
            .client
            .clone();
        spawn(async move {
            match client
                .execute(StartTraktAuth)
                .await
            {
                Ok(resp) => {
                    if let Some(win) = web_sys::window() {
                        let _ = win.open_with_url(&resp.verification_url);
                    }
                    state.set(TraktConnectionState::Connecting {
                        user_code: resp.user_code.clone(),
                        verification_url: resp.verification_url.clone(),
                    });

                    let interval_secs = resp.interval.max(1) as u64;
                    let max_attempts = (resp.expires_in.max(1) as u64 / interval_secs) + 1;
                    let poll_client = client.clone();
                    spawn(async move {
                        for _ in 0..max_attempts {
                            gloo_timers::future::sleep(std::time::Duration::from_secs(
                                interval_secs,
                            ))
                            .await;
                            match poll_client
                                .execute(PollTraktAuth)
                                .await
                            {
                                Ok(poll_resp) => match poll_resp.status.as_str() {
                                    "authorized" => {
                                        state.set(TraktConnectionState::Connected);
                                        return;
                                    }
                                    "expired" | "denied" => {
                                        state.set(TraktConnectionState::Error(
                                            "Code expired or denied — try again.".to_string(),
                                        ));
                                        return;
                                    }
                                    _ => {}
                                },
                                Err(_) => {}
                            }
                        }
                        state.set(TraktConnectionState::Error(
                            "Code expired or denied — try again.".to_string(),
                        ));
                    });
                }
                Err(e) => state.set(TraktConnectionState::Error(e.user_message())),
            }
        });
    };

    let app_state_disconnect = app_state.clone();
    let on_disconnect = move |_| {
        let client = app_state_disconnect
            .client
            .clone();
        disconnect_error.set(None);
        spawn(async move {
            match client
                .execute(DisconnectTraktAuth)
                .await
            {
                Ok(_) => state.set(TraktConnectionState::NotConnected),
                Err(e) => disconnect_error.set(Some(e.user_message())),
            }
        });
    };

    rsx! {
        Card { title: "Trakt Account",
            match &*state.read() {
                TraktConnectionState::Loading => rsx! { LoadingText {} },
                TraktConnectionState::NotConnected => rsx! {
                    div { style: "display:flex;flex-direction:column;gap:14px",
                        span { class: "task-badge task-badge-idle", "Not connected" }
                        p { class: "field-hint",
                            "Link your Trakt.tv account to automatically scrobble what you watch."
                        }
                        button {
                            r#type: "button",
                            class: "btn btn-primary",
                            onclick: on_connect,
                            "Connect Trakt account"
                        }
                    }
                },
                TraktConnectionState::Connecting { user_code, verification_url } => rsx! {
                    div { style: "display:flex;flex-direction:column;gap:14px",
                        span { class: "task-badge task-badge-running", "Waiting for approval…" }
                        div {
                            style: "font-size:2rem;font-family:monospace;letter-spacing:.2em;text-align:center",
                            "{user_code}"
                        }
                        p { class: "field-hint",
                            "Enter this code at "
                            a { href: "{verification_url}", target: "_blank", "{verification_url}" }
                        }
                        LoadingText {}
                    }
                },
                TraktConnectionState::Connected => rsx! {
                    div { style: "display:flex;flex-direction:column;gap:14px",
                        span { class: "task-badge task-badge-completed", "Connected" }
                        if let Some(err) = disconnect_error.read().as_ref() {
                            ErrorAlert { message: err.clone() }
                        }
                        button {
                            r#type: "button",
                            class: "btn btn-secondary",
                            onclick: on_disconnect,
                            "Disconnect"
                        }
                    }
                },
                TraktConnectionState::Error(message) => rsx! {
                    div { style: "display:flex;flex-direction:column;gap:14px",
                        ErrorAlert { message: message.clone() }
                        button {
                            r#type: "button",
                            class: "btn btn-secondary",
                            onclick: move |_| state.set(TraktConnectionState::NotConnected),
                            "Retry"
                        }
                    }
                },
            }
        }
    }
}
```

**Note on the polling loop:** unlike `components/tasks.rs`'s infinite loop, this one is bounded (`max_attempts` derived from `expires_in / interval`) as a safety net in case the backend never returns a terminal `"expired"`/`"denied"` status before the device code actually expires. No cleanup-on-unmount is implemented, consistent with every other polling example in this codebase (`components/tasks.rs`, `components/server_info.rs`) — an accepted, pre-existing limitation, not a regression introduced here.

- [ ] **Step 2: Register the module and export the component**

In `crates/remux-dashboard/src/pages/mod.rs`:

```rust
pub mod addons;
pub mod api_keys;
pub mod branding;
pub mod collections;
pub mod dashboard;
pub mod integrations;
pub mod iptv;
pub mod settings;
pub mod streams;
pub mod users;

pub use addons::AddonsPage;
pub use api_keys::ApiKeysPage;
pub use branding::BrandingPage;
pub use collections::CollectionsPage;
pub use dashboard::DashboardPage;
pub use integrations::TraktAccountCard;
pub use iptv::IptvPage;
pub use settings::{
    IntroSettingsCard, JellyfinImportCard, P2pSettingsCard, PlaybackSettingsCard,
    ProbeSettingsCard, RemuxdbSettingsCard, SearchSettingsCard, ServerSettingsCard,
    TraktSettingsCard,
};
pub use streams::StreamGroupsCard;
pub use users::UsersPage;
```

- [ ] **Step 3: Add the route**

In `crates/remux-dashboard/src/router.rs`, add a new variant (anywhere in the enum, e.g. right after `ActivityRoute` and before `NotFound`):

```rust
    #[route("/activity")]
    ActivityRoute,
    #[route("/integrations/trakt")]
    IntegrationsTraktRoute,
```

And its wrapper component:

```rust
#[component]
pub(crate) fn IntegrationsTraktRoute() -> Element {
    let app_state = use_context::<AppState>();
    rsx! { TraktAccountCard { app_state } }
}
```

- [ ] **Step 4: Add the sidebar group**

In `crates/remux-dashboard/src/layout.rs`:

1. Add a `page_title` match arm (near `Route::ActivityRoute => "Activity",` — note this arm doesn't currently exist as written in the file; add it next to the other route arms):

```rust
        Route::IntegrationsTraktRoute => "Trakt",
```

2. Add a new `SidebarGroup`, placed after the `nav-divider` that currently precedes the "Activity" `NavItem` (around line 254-260) — this group is visible to every logged-in user, unlike "Settings"/"Access" which fail server-side for non-admins:

```rust
                    div { class: "nav-divider" }

                    SidebarGroup {
                        label: "Integrations",
                        active: matches!(route, Route::IntegrationsTraktRoute),
                        NavSubItem {
                            label: "Trakt",
                            active: route == Route::IntegrationsTraktRoute,
                            on_click: move |_| { navigator().push(Route::IntegrationsTraktRoute); sidebar_open.set(false); },
                        }
                    }

                    NavItem {
                        label: "Activity",
                        active: route == Route::ActivityRoute,
                        on_click: move |_| { navigator().push(Route::ActivityRoute); sidebar_open.set(false); },
                    }
```

(This replaces the existing lone `div { class: "nav-divider" }` immediately before the "Activity" `NavItem` — do not create a duplicate divider.)

- [ ] **Step 5: Verify it builds**

Run: `cargo check -p remux-dashboard`
Expected: 0 errors.

- [ ] **Step 6: Commit**

```bash
git add crates/remux-dashboard/src/pages/integrations.rs crates/remux-dashboard/src/pages/mod.rs crates/remux-dashboard/src/router.rs crates/remux-dashboard/src/layout.rs
git commit -m "feat(trakt-ui): add self-service Trakt account connect/disconnect card"
```

---

### Task 4: Full workspace verification and manual browser test

**Files:** none (verification only).

- [ ] **Step 1: Full build**

Run: `cargo build --workspace`
Expected: 0 errors.

- [ ] **Step 2: Full test suite**

Run: `cargo test --workspace -- --test-threads=4`
Expected: PASS, same count as the pre-existing baseline (this plan adds no new automated tests, per the Global Constraints note).

- [ ] **Step 3: Manual browser test scenario**

1. Register a Trakt app at `https://trakt.tv/oauth/applications`.
2. Start the server (`cargo run -p remux-server`) and build/serve the dashboard per this project's normal dev workflow (`dx serve` in `crates/remux-dashboard`, or however this repo's `run` skill launches it).
3. Log into the dashboard as an admin. Navigate to **Settings → Trakt**. Enter the Client ID/Secret, click **Save** — expect the "Settings saved." success message.
4. Navigate to **Integrations → Trakt**. Expect a "Not connected" badge and a **Connect Trakt account** button.
5. Click **Connect Trakt account** — the device code and a clickable verification link appear in the card; a new tab may also open automatically if the browser doesn't block the popup (popup blockers commonly prevent this since the open call happens after an async await, outside the original click's gesture window — this is expected, not a bug; the clickable link is the reliable fallback).
6. In the opened tab, enter the code and approve. Within one polling interval, the dashboard card should flip to a "Connected" badge with a **Disconnect** button, without a page reload.
7. Play something through the normal Jellyfin-compatible client flow, and check `https://trakt.tv/users/me/watching` — confirms Task 8 of the backend plan (`docs/superpowers/plans/2026-07-29-trakt-scrobbling.md`) is reachable end-to-end from the UI now, closing the gap the final backend review flagged ("feature unreachable without hand-crafted API calls").
8. Click **Disconnect** — expect the card to return to "Not connected", and confirm on `https://trakt.tv/settings/applications` that the authorization was revoked.
9. As a non-admin user (or by hitting `/settings/trakt` while logged in as one), confirm **Integrations → Trakt** still works normally (backend uses `AuthSession`, not `AdminSession`, for all four endpoints) while **Settings → Trakt** fails to load (backend `AdminSession` gate) — this confirms the permission split the design called for.

This step has no automated assertion — it's the manual confirmation that closes out both the backend plan's and this UI plan's outstanding "no way to test as a real user" gap.

- [ ] **Step 4: No commit** — this task only verifies; if any step fails, fix forward in the task that introduced the regression and re-commit there.
