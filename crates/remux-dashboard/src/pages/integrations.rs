use crate::{
    components::{Card, ErrorAlert, LoadingText},
    state::AppState,
};
use dioxus::prelude::*;
use remux_sdks::remux::{
    DisconnectTraktAuth, GetTraktAuthStatus, PollTraktAuth, StartTraktAuth,
};

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
                Ok(resp) if resp.connected => {
                    state.set(TraktConnectionState::Connected)
                }
                Ok(_) => state.set(TraktConnectionState::NotConnected),
                Err(e) => {
                    let msg = e.user_message();
                    if msg.contains("client_id is not configured") {
                        state.set(TraktConnectionState::Error(
                            "Trakt is not configured by the server administrator."
                                .to_string(),
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
                        user_code: resp
                            .user_code
                            .clone(),
                        verification_url: resp
                            .verification_url
                            .clone(),
                    });

                    let interval_secs = resp
                        .interval
                        .max(1) as u64;
                    let max_attempts = (resp
                        .expires_in
                        .max(1) as u64
                        / interval_secs)
                        + 1;
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
                                Ok(poll_resp) => match poll_resp
                                    .status
                                    .as_str()
                                {
                                    "authorized" => {
                                        state.set(TraktConnectionState::Connected);
                                        return;
                                    }
                                    "expired" | "denied" => {
                                        state.set(TraktConnectionState::Error(
                                            "Code expired or denied — try again."
                                                .to_string(),
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
