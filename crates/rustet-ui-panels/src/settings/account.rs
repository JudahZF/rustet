//! Settings → Account: the Apple Music account, the lossless (FairPlay) details, the engine.
//! The sign-in sheet is the shell's: "Sign in" calls `AccountModel::sign_in`.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gpui::{AnyElement, Context, Window};
use rustet_app::account::lossless::SignOutResult;
use rustet_app::account::{AccountState, Backend, SigningIn};
use rustet_app::engine::Connection;
use rustet_app::engine::types::{DrmStatus, FairPlayState, SessionState};
use rustet_ui::components::{Button, Toast};

use super::SettingsPage;
use crate::confirm::confirm;
use crate::format;
use crate::ui::{self, fact, group, row, run_in};

/// How long after `now_ms` (Unix ms) the FairPlay session must be renewed: from when it
/// became ready plus its time to live, else its time to live less its age. `None` when the
/// status does not say (a `drm` event carries neither).
pub fn lease_left(drm: &DrmStatus, now_ms: i64) -> Option<Duration> {
    let ttl = drm.session_ttl_ms?;
    let left = match (drm.ready_since_ms, drm.session_age_ms) {
        (Some(since), _) => since + ttl - now_ms,
        (None, Some(age)) => ttl - age,
        (None, None) => return None,
    };
    Some(Duration::from_millis(u64::try_from(left).unwrap_or(0)))
}

fn fairplay_text(s: FairPlayState) -> &'static str {
    match s {
        FairPlayState::Ready => "Ready",
        FairPlayState::Initializing => "Starting",
        FairPlayState::Failed => "Failed. Sign in again.",
        FairPlayState::Unknown => "Not started",
    }
}

fn session_text(s: SessionState) -> &'static str {
    match s {
        SessionState::Valid => "Active",
        SessionState::Expired => "Expired. Rustet renews it when it next plays lossless.",
        SessionState::Empty | SessionState::Unknown => "None",
    }
}

fn connection_text(c: &Connection) -> String {
    match c {
        Connection::Starting => "Starting".to_owned(),
        Connection::Ready => "Running".to_owned(),
        Connection::Restarting { retry_in: Some(d) } => {
            format!("Restarting in {} s", d.as_secs().max(1))
        }
        Connection::Restarting { retry_in: None } => "Restarting".to_owned(),
        Connection::Failed { reason } => reason.clone(),
        Connection::Stopped => "Stopped".to_owned(),
    }
}

fn sign_out_toast(result: SignOutResult) -> Toast {
    match result {
        SignOutResult::Done => Toast::new("Signed out"),
        SignOutResult::DoneWithNote(note) => Toast::new(note),
        SignOutResult::Failed(why) => Toast::failure(why),
    }
}

impl SettingsPage {
    fn sign_in(&mut self, _: &mut Window, _: &mut Context<Self>) {
        let app = self.app.clone();
        self.app
            .runtime()
            .spawn(async move { app.account().sign_in().await });
    }

    fn sign_out(&mut self, forget_device: bool, window: &mut Window, cx: &mut Context<Self>) {
        let app = self.app.clone();
        run_in(
            self.app.runtime(),
            async move { app.account().sign_out(forget_device).await },
            window,
            cx,
            |_, result, window, cx| Self::toast(sign_out_toast(result), window, cx),
        );
    }

    fn confirm_forget(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let page = cx.entity();
        confirm(
            "Forget this device?",
            "Rustet signs out and removes this computer from your Apple ID's devices. \
             Next time, you sign in with your password and a verification code.",
            "Forget this device",
            move |window, cx| page.update(cx, |p, cx| p.sign_out(true, window, cx)),
            window,
            cx,
        );
    }

    pub(super) fn account(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let fairplay = self.status.backend == Some(Backend::FairPlay);
        let sign_in = || Button::new("sign-in", "Sign in").primary();
        let account_rows: Vec<AnyElement> = match &self.status.account {
            AccountState::Starting => vec![fact("Status", "Waiting for Rustet's engine", cx)],
            AccountState::SignedOut => vec![row(
                "Not signed in",
                Some("Sign in with your Apple ID to play your library and lossless audio.".into()),
                sign_in().on_click(self.on(Self::sign_in, cx)),
                cx,
            )],
            AccountState::SigningIn(how) => vec![fact(
                "Status",
                match how {
                    SigningIn::Sheet(_) => "Signing in",
                    SigningIn::WebWindow => "Signing in, in the Apple Music window",
                },
                cx,
            )],
            AccountState::SignedIn { storefront } => {
                let mut rows = vec![row(
                    "Signed in",
                    Some(format!("Storefront: {}", storefront.to_uppercase()).into()),
                    Button::new("sign-out", "Sign out")
                        .on_click(self.on(|p, w, cx| p.sign_out(false, w, cx), cx)),
                    cx,
                )];
                if fairplay {
                    rows.push(row(
                        "Forget this device",
                        Some(
                            "Signs out and removes this computer from your Apple ID's devices."
                                .into(),
                        ),
                        Button::new("forget-device", "Forget this device")
                            .on_click(self.on(Self::confirm_forget, cx)),
                        cx,
                    ));
                }
                rows
            }
            AccountState::Error { message } => vec![row(
                "Signed out",
                Some(message.clone().into()),
                sign_in().on_click(self.on(Self::sign_in, cx)),
                cx,
            )],
        };

        let mut groups = vec![group("Apple Music", account_rows, cx)];
        groups.push(self.lossless_group(cx));

        let mut engine = vec![fact("Engine", connection_text(&self.status.connection), cx)];
        if matches!(self.status.connection, Connection::Failed { .. }) {
            let app = self.app.clone();
            engine.push(row(
                "Start the engine again",
                None,
                Button::new("retry-engine", "Try again").on_click(move |_, _, _| {
                    let app = app.clone();
                    app.runtime()
                        .clone()
                        .spawn(async move { app.retry_engine().await });
                }),
                cx,
            ));
        }
        groups.push(group("Rustet's engine", engine, cx));
        groups
    }

    fn lossless_group(&self, cx: &mut Context<Self>) -> AnyElement {
        let rows = match (&self.status.drm, self.status.backend) {
            (Some(drm), _) if !drm.available => vec![ui::note(NO_FAIRPLAY, cx)],
            (_, Some(Backend::Web)) => vec![ui::note(NO_FAIRPLAY, cx)],
            (None, _) => vec![fact("Status", "Waiting for Rustet's engine", cx)],
            (Some(drm), _) => {
                let formats = if drm.capabilities.hi_res {
                    "ALAC up to 24-bit 192 kHz"
                } else if drm.capabilities.alac || drm.capabilities.cbcs {
                    "ALAC up to 24-bit 48 kHz"
                } else {
                    "Not available until you sign in"
                };
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default();
                let lease = lease_left(drm, now.as_millis() as i64).map(|left| {
                    if left.is_zero() {
                        return "Expired".to_owned();
                    }
                    let at = chrono::DateTime::from_timestamp((now + left).as_secs() as i64, 0)
                        .map(|t| {
                            t.with_timezone(&chrono::Local)
                                .format(" (%-d %b, %H:%M)")
                                .to_string()
                        })
                        .unwrap_or_default();
                    format!("In {}{at}", format::span(left))
                });
                let mut rows = vec![
                    fact("FairPlay", fairplay_text(drm.state.fairplay), cx),
                    fact("Session", session_text(drm.state.session), cx),
                ];
                rows.extend(lease.map(|lease| fact("Session renewal", lease, cx)));
                rows.push(fact("Formats", formats, cx));
                rows
            }
        };
        group("Lossless", rows, cx)
    }
}

const NO_FAIRPLAY: &str = "Lossless needs Apple's FairPlay libraries, which this computer does not have. \
                           Rustet plays AAC 256 kbps.";

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use rustet_app::engine::types::DrmStatus;

    use super::lease_left;

    #[test]
    fn renewal_comes_from_ready_since_or_age_else_nothing() {
        let hour: i64 = 3_600_000;
        let ms = |v: i64| Duration::from_millis(v as u64);
        let ready = DrmStatus {
            ready_since_ms: Some(1_000),
            session_ttl_ms: Some(24 * hour),
            ..DrmStatus::default()
        };
        assert_eq!(lease_left(&ready, 1_000 + hour), Some(ms(23 * hour)));
        let aged = DrmStatus {
            session_age_ms: Some(hour),
            session_ttl_ms: Some(2 * hour),
            ..DrmStatus::default()
        };
        assert_eq!(lease_left(&aged, 0), Some(ms(hour)));
        let expired = DrmStatus {
            ready_since_ms: Some(0),
            session_ttl_ms: Some(hour),
            ..DrmStatus::default()
        };
        assert_eq!(lease_left(&expired, 2 * hour), Some(Duration::ZERO));
        assert_eq!(
            lease_left(&DrmStatus::default(), 0),
            None,
            "an event carries neither"
        );
    }
}
