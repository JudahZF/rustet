//! Settings → Integrations: Last.fm and ListenBrainz (their own state, in the encrypted
//! store) and Discord (a setting).
//!
//! Last.fm: the user's API key and secret, then "Connect" opens Last.fm's authorize page in
//! the browser and "Finish connecting" trades the approved token for a session.
//! ListenBrainz: the user token; its status asks ListenBrainz who it belongs to.

use gpui::{AnyElement, Context, IntoElement, Window};
use rustet_ui::components::Button;
use rustet_ui::host::{Host, HostRequest};
use serde_json::{Value, json};

use super::SettingsPage;
use crate::ui::{self, fact, group, row, run, run_in};

impl SettingsPage {
    pub(super) fn refresh_integrations(&mut self, cx: &mut Context<Self>) {
        self.status.lastfm = Some(self.app.scrobblers().lastfm.status());
        let app = self.app.clone();
        run(
            self.app.runtime(),
            async move { app.scrobblers().listenbrainz.status().await },
            cx,
            |p, s, _| {
                if !p.frozen {
                    p.status.listenbrainz = Some(s);
                }
            },
        );
    }

    fn lastfm_save(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let key = self.c.lastfm_key.read(cx).text().trim().to_owned();
        let secret = self.c.lastfm_secret.read(cx).text().trim().to_owned();
        if key.is_empty() || secret.is_empty() {
            self.refuse(
                "lastfm",
                "Enter both the API key and the shared secret from your Last.fm API account.",
                cx,
            );
            return;
        }
        // The scrobblers save in the encrypted store, which writes on the app's runtime.
        let _runtime = self.app.runtime().enter();
        self.app
            .scrobblers()
            .lastfm
            .set_credentials(&json!({ "apiKey": key, "secret": secret }));
        self.c.lastfm_secret.update(cx, |i, cx| i.set_text("", cx));
        self.refused.remove("lastfm");
        self.refresh_integrations(cx);
    }

    fn lastfm_connect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let app = self.app.clone();
        run_in(
            self.app.runtime(),
            async move { app.scrobblers().lastfm.auth().await },
            window,
            cx,
            |p, result, window, cx| match result {
                Ok(start) => {
                    p.refused.remove("lastfm");
                    p.status.lastfm_waiting = true;
                    Host::request(HostRequest::OpenExternal(start.url.to_string()), window, cx);
                }
                Err(e) => p.refuse(
                    "lastfm",
                    format!(
                        "Last.fm did not start the sign-in: {e}. Check the API key and secret."
                    ),
                    cx,
                ),
            },
        );
    }

    fn lastfm_finish(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let app = self.app.clone();
        run(
            self.app.runtime(),
            async move { app.scrobblers().lastfm.session(&Value::Null).await },
            cx,
            |p, result, cx| {
                match result {
                    Ok(_) => {
                        p.status.lastfm_waiting = false;
                        p.refused.remove("lastfm");
                    }
                    Err(e) => p.refuse("lastfm", format!("Not connected yet: {e}."), cx),
                }
                p.refresh_integrations(cx);
            },
        );
    }

    fn listenbrainz_connect(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let token = self.c.listenbrainz_token.read(cx).text().trim().to_owned();
        if token.is_empty() {
            self.refuse(
                "listenbrainz",
                "Paste your user token from listenbrainz.org/settings.",
                cx,
            );
            return;
        }
        let _runtime = self.app.runtime().enter();
        if !self
            .app
            .scrobblers()
            .listenbrainz
            .set_token(&Value::String(token))
            .ok
        {
            self.refuse(
                "listenbrainz",
                "That token is too long. Paste the user token from listenbrainz.org/settings.",
                cx,
            );
            return;
        }
        self.c
            .listenbrainz_token
            .update(cx, |i, cx| i.set_text("", cx));
        self.refused.remove("listenbrainz");
        self.refresh_integrations(cx);
    }

    pub(super) fn integrations(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut lastfm = Vec::new();
        match &self.status.lastfm {
            None => lastfm.push(fact("Status", "Checking", cx)),
            Some(s) if s.connected => lastfm.push(row(
                format!(
                    "Connected as {}",
                    s.username.as_deref().unwrap_or("your Last.fm account")
                ),
                Some("Tracks you play are scrobbled after half their length or 4 minutes.".into()),
                Button::new("lastfm-disconnect", "Disconnect").on_click(self.on(
                    |p, _, cx| {
                        // The scrobblers save in the encrypted store, which writes on the app's runtime.
                        let _runtime = p.app.runtime().enter();
                        p.app.scrobblers().lastfm.disconnect();
                        p.refresh_integrations(cx);
                    },
                    cx,
                )),
                cx,
            )),
            Some(s) if s.configured => {
                let finish = self.status.lastfm_waiting;
                lastfm.push(row(
                    "Not connected",
                    Some(if finish {
                        "Approve Rustet on the Last.fm page in your browser, then finish here."
                            .into()
                    } else {
                        format!("API key {}", s.api_key).into()
                    }),
                    if finish {
                        Button::new("lastfm-finish", "Finish connecting")
                            .primary()
                            .on_click(self.on(Self::lastfm_finish, cx))
                    } else {
                        Button::new("lastfm-connect", "Connect")
                            .primary()
                            .on_click(self.on(Self::lastfm_connect, cx))
                    },
                    cx,
                ));
            }
            Some(_) => {
                lastfm.push(ui::note(
                    "Last.fm needs your own API account: create one at last.fm/api, then enter its key and secret.",
                    cx,
                ));
                lastfm.push(row("API key", None, ui::field(&self.c.lastfm_key, cx), cx));
                lastfm.push(row(
                    "Shared secret",
                    None,
                    ui::field(&self.c.lastfm_secret, cx),
                    cx,
                ));
                lastfm.push(row(
                    "Save the key and secret",
                    None,
                    Button::new("lastfm-save", "Save").on_click(self.on(Self::lastfm_save, cx)),
                    cx,
                ));
            }
        }
        lastfm.extend(self.refusal_of("lastfm", cx));

        let mut listenbrainz = Vec::new();
        match &self.status.listenbrainz {
            None => listenbrainz.push(fact("Status", "Checking", cx)),
            Some(s) if s.connected => listenbrainz.push(row(
                match &s.username {
                    Some(user) => format!("Connected as {user}"),
                    None => "Connected".to_owned(),
                },
                Some("Tracks you play are submitted as listens.".into()),
                Button::new("lb-disconnect", "Disconnect").on_click(self.on(
                    |p, _, cx| {
                        // The scrobblers save in the encrypted store, which writes on the app's runtime.
                        let _runtime = p.app.runtime().enter();
                        p.app.scrobblers().listenbrainz.disconnect();
                        p.refresh_integrations(cx);
                    },
                    cx,
                )),
                cx,
            )),
            Some(_) => {
                listenbrainz.push(row(
                    "User token",
                    Some("From listenbrainz.org/settings.".into()),
                    ui::field(&self.c.listenbrainz_token, cx),
                    cx,
                ));
                listenbrainz.push(row(
                    "Connect ListenBrainz",
                    None,
                    Button::new("lb-connect", "Connect")
                        .primary()
                        .on_click(self.on(Self::listenbrainz_connect, cx)),
                    cx,
                ));
            }
        }
        listenbrainz.extend(self.refusal_of("listenbrainz", cx));

        let mut discord = vec![
            self.toggle(
                "discord",
                "Discord",
                self.settings.integrations.discord,
                |s, on| s.integrations.discord = on,
                cx,
            )
            .description("Show what you play in your Discord status. Discord must be running.")
            .into_any_element(),
        ];
        discord.extend(self.refusal_of("discord", cx));

        vec![
            group("Last.fm", lastfm, cx),
            group("ListenBrainz", listenbrainz, cx),
            group("Discord Rich Presence", discord, cx),
        ]
    }
}
