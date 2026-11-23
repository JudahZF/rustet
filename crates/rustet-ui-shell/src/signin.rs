//! The sign-in sheet (spec §3.3, D3), drawn from the account model's [`SheetView`]: email and
//! password, the six-digit code with resend and "text me", Apple's options, and errors that
//! say what to do. The model runs the sign-in; the sheet only draws it and hands input over.

use gpui::{
    AnyElement, App, Context, Entity, Focusable, Role, SharedString, Subscription, Task, Window,
    div, prelude::*,
};
use rustet_app::account::AccountModel;
use rustet_app::account::lossless::{CodeWait, PASSWORD_REVEAL, SheetError, SheetView};
use rustet_ui::components::{
    Button, CodeInput, CodeInputEvent, Sheet, TextInput, TextInputEvent, Toggle,
};
use rustet_ui::theme::ActiveTheme;

/// The page of the sheet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Page {
    /// Email and password; `email_editable` is false for the account signed in before.
    SignIn {
        email_editable: bool,
    },
    Code {
        phone_options: bool,
    },
    Choice {
        options: Vec<String>,
    },
}

/// What the sheet draws for a [`SheetView`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SheetDrawing {
    pub title: String,
    pub message: String,
    pub page: Page,
    /// "Signing in…", "Verifying…": the inputs wait.
    pub busy: Option<&'static str>,
    pub error: Option<SheetError>,
    /// The digits must go: the code was wrong.
    pub clear_code: bool,
}

/// The drawing of `view`; `None` when the sheet is closed.
pub fn sheet_drawing(view: &SheetView) -> Option<SheetDrawing> {
    Some(match view {
        SheetView::Closed => return None,
        SheetView::SignIn {
            email,
            known_account,
            busy,
            error,
        } => SheetDrawing {
            title: if *known_account {
                "Sign-in required".to_owned()
            } else {
                "Sign in with your Apple Account".to_owned()
            },
            message: if *known_account {
                format!("Enter the password for your Apple Account {email}.")
            } else {
                "Rustet plays your library, and lossless and hi-res audio, with your Apple Music account.".to_owned()
            },
            page: Page::SignIn {
                email_editable: !*known_account,
            },
            busy: busy.then_some("Signing in…"),
            error: error.clone(),
            clear_code: false,
        },
        SheetView::Code {
            busy,
            error,
            wrong_code,
            phone_options,
        } => SheetDrawing {
            title: "Enter verification code".to_owned(),
            message: "A verification code was sent to your other devices. Enter it to continue."
                .to_owned(),
            page: Page::Code {
                phone_options: *phone_options,
            },
            busy: busy.map(|w| match w {
                CodeWait::Verifying => "Verifying…",
                CodeWait::Resending => "Resending…",
                CodeWait::SendingSms => "Sending SMS…",
            }),
            error: error.clone(),
            clear_code: *wrong_code,
        },
        SheetView::Choice {
            title,
            options,
            busy,
        } => SheetDrawing {
            title: title.clone(),
            message: "Choose how Apple gets the code to you.".to_owned(),
            page: Page::Choice {
                options: options.clone(),
            },
            busy: busy.then_some("Sending…"),
            error: None,
            clear_code: false,
        },
    })
}

/// What the sheet asks the account model for; the shell runs it
/// ([`SignInRequest::run`]), and the outcome comes back as a new [`SheetView`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignInRequest {
    SignIn {
        email: String,
        password: String,
        remember: bool,
    },
    Code(String),
    ResendCode,
    TogglePhoneOptions,
    TextMeACode,
    Choose(usize),
    Cancel,
}

impl SignInRequest {
    pub fn run(self, account: &AccountModel, cx: &App) {
        let lossless = account.lossless().clone();
        match self {
            Self::SignIn {
                email,
                password,
                remember,
            } => spawn(cx, async move {
                lossless.sign_in(&email, &password, remember).await
            }),
            Self::Code(code) => spawn(cx, async move { lossless.submit_code(&code).await }),
            Self::ResendCode => spawn(cx, async move { lossless.resend_code().await }),
            Self::TogglePhoneOptions => lossless.toggle_phone_options(),
            Self::TextMeACode => spawn(cx, async move { lossless.text_me_a_code().await }),
            Self::Choose(i) => spawn(cx, async move { lossless.choose(i).await }),
            Self::Cancel => spawn(cx, async move { lossless.cancel().await }),
        }
    }
}

impl gpui::EventEmitter<SignInRequest> for SignInSheet {}

pub struct SignInSheet {
    view: SheetView,
    pub(crate) email: Entity<TextInput>,
    pub(crate) password: Entity<TextInput>,
    code: Entity<CodeInput>,
    remember: bool,
    revealed: bool,
    reveal: Option<Task<()>>,
    /// Focus the page's first field at the next draw.
    focus_pending: bool,
    /// Enter in the email field with no password yet: the password field takes the focus.
    focus_password: bool,
    _subscriptions: Vec<Subscription>,
}

impl SignInSheet {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let email = cx.new(|cx| TextInput::new("Email or phone number", cx));
        let password = cx.new(|cx| TextInput::new("Password", cx).password());
        let code = cx.new(CodeInput::new);
        let subscriptions = vec![
            cx.subscribe(&email, |this, _, e: &TextInputEvent, cx| {
                if matches!(e, TextInputEvent::Submitted(_)) {
                    if this.password.read(cx).text().is_empty() {
                        this.focus_password = true;
                        cx.notify();
                    } else {
                        this.submit(cx);
                    }
                }
            }),
            cx.subscribe(&password, |this, _, e: &TextInputEvent, cx| {
                if matches!(e, TextInputEvent::Submitted(_)) {
                    this.submit(cx);
                }
            }),
            cx.subscribe(&code, |this, _, e: &CodeInputEvent, cx| {
                let CodeInputEvent::Complete(code) = e;
                this.submit_code(code.clone(), cx);
            }),
        ];
        Self {
            view: SheetView::Closed,
            email,
            password,
            code,
            remember: false,
            revealed: false,
            reveal: None,
            focus_pending: false,
            focus_password: false,
            _subscriptions: subscriptions,
        }
    }

    /// The model's sheet changed.
    pub fn set_view(&mut self, view: SheetView, cx: &mut Context<Self>) {
        let same_page = std::mem::discriminant(&view) == std::mem::discriminant(&self.view);
        match &view {
            SheetView::SignIn { email, busy, .. } => {
                if !same_page {
                    let email = email.clone();
                    self.email.update(cx, |e, cx| e.set_text(email, cx));
                    self.password.update(cx, |p, cx| p.set_text("", cx));
                    self.focus_pending = true;
                }
                let busy = *busy;
                self.password.update(cx, |p, cx| p.set_disabled(busy, cx));
                self.email.update(cx, |e, cx| e.set_disabled(busy, cx));
            }
            SheetView::Code { wrong_code, .. } => {
                if !same_page || *wrong_code {
                    self.code.update(cx, |c, cx| c.clear(cx));
                    self.focus_pending = true;
                }
                // The password is not needed any more.
                self.password.update(cx, |p, cx| p.set_text("", cx));
            }
            SheetView::Choice { .. } | SheetView::Closed => {
                self.password.update(cx, |p, cx| p.set_text("", cx));
            }
        }
        self.view = view;
        cx.notify();
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        let SheetView::SignIn {
            email,
            known_account,
            busy: false,
            ..
        } = &self.view
        else {
            return;
        };
        let email = if *known_account {
            email.clone()
        } else {
            self.email.read(cx).text().trim().to_owned()
        };
        let password = self.password.read(cx).text().to_string();
        if email.is_empty() || password.is_empty() {
            return;
        }
        cx.emit(SignInRequest::SignIn {
            email,
            password,
            remember: self.remember,
        });
    }

    fn submit_code(&mut self, code: String, cx: &mut Context<Self>) {
        if !matches!(self.view, SheetView::Code { busy: None, .. }) {
            return;
        }
        cx.emit(SignInRequest::Code(code));
    }

    fn reveal(&mut self, cx: &mut Context<Self>) {
        self.revealed = !self.revealed;
        let on = self.revealed;
        self.password.update(cx, |p, cx| p.set_revealed(on, cx));
        // The password hides itself again after a few seconds.
        self.reveal = on.then(|| {
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(PASSWORD_REVEAL).await;
                this.update(cx, |s, cx| {
                    s.revealed = false;
                    s.password.update(cx, |p, cx| p.set_revealed(false, cx));
                    cx.notify();
                })
                .ok();
            })
        });
        cx.notify();
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        cx.emit(SignInRequest::Cancel);
    }
}

/// Runs a model call on the app's runtime; its outcome arrives as a new sheet state.
fn spawn(cx: &App, f: impl Future<Output = ()> + Send + 'static) {
    let job = rustet_ui::runtime::spawn(cx, f);
    cx.background_executor()
        .spawn(async move { drop(job.await) })
        .detach();
}

fn error_line(error: &SheetError, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let url = error.help_url.clone();
    div()
        .id("sign-in-error")
        .role(Role::Alert)
        .aria_label(SharedString::from(error.message.clone()))
        .flex()
        .flex_col()
        .gap(theme.space.xs)
        .p(theme.space.m)
        .rounded(theme.radius.menu)
        .bg(theme.colors.fill)
        .text_size(theme.text.body)
        .text_color(theme.colors.text)
        .child(
            div()
                .flex()
                .gap(theme.space.s)
                .child(
                    rustet_ui::icons::Icon::Alert
                        .element()
                        .flex_none()
                        .size(theme.size.icon_small)
                        .text_color(theme.colors.text_secondary),
                )
                .child(div().flex_1().min_w_0().child(error.message.clone())),
        )
        .children(url.map(|url| {
            let label = format!(
                "Open {}",
                url.trim_start_matches("https://").trim_end_matches('/')
            );
            div().flex().child(
                Button::new("sign-in-help", label).on_click(move |_, _, cx| cx.open_url(&url)),
            )
        }))
        .into_any_element()
}

impl Render for SignInSheet {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let colors = theme.colors;
        let Some(d) = sheet_drawing(&self.view) else {
            return div().into_any_element();
        };
        if std::mem::take(&mut self.focus_password) {
            window.focus(&self.password.focus_handle(cx), cx);
        }
        // The page's first field: focused now when the page changes, and by the sheet when it
        // first appears (it would take the focus for its panel otherwise).
        let mut open_on = None;
        if std::mem::take(&mut self.focus_pending) {
            let target = match &d.page {
                Page::SignIn {
                    email_editable: true,
                } if self.email.read(cx).text().is_empty() => Some(self.email.focus_handle(cx)),
                Page::SignIn { .. } => Some(self.password.focus_handle(cx)),
                Page::Code { .. } => Some(self.code.focus_handle(cx)),
                Page::Choice { .. } => None,
            };
            if let Some(target) = &target {
                window.focus(target, cx);
            }
            open_on = target;
        }
        let busy = d.busy.is_some();
        let entity = cx.entity();
        let note = |text: &'static str| {
            div()
                .text_size(theme.text.callout)
                .text_color(colors.text_secondary)
                .child(text)
        };
        let mut sheet = Sheet::new("sign-in", d.title.clone())
            .width(440.)
            .when_some(open_on, Sheet::focus_on_open)
            .on_dismiss(move |_, cx| entity.update(cx, |s, cx| s.cancel(cx)))
            .child(
                div()
                    .text_size(theme.text.body)
                    .line_height(theme.text.body_line)
                    .text_color(colors.text_secondary)
                    .child(d.message.clone()),
            );

        let body: Vec<AnyElement> = match &d.page {
            Page::SignIn { email_editable } => {
                let ready = !self.password.read(cx).text().is_empty()
                    && (!email_editable || !self.email.read(cx).text().trim().is_empty());
                vec![
                    div()
                        .flex()
                        .flex_col()
                        .gap(theme.space.s)
                        .when(*email_editable, |d| d.child(self.email.clone()))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(theme.space.s)
                                .child(div().flex_1().child(self.password.clone()))
                                .child(
                                    Button::new("reveal", if self.revealed { "Hide" } else { "Show" })
                                        .disabled(busy)
                                        .on_click(cx.listener(|this, _, _, cx| this.reveal(cx))),
                                ),
                        )
                        .into_any_element(),
                    Toggle::new("remember", "Remember password on this computer", self.remember)
                        .description("Your password is sent only to Apple. Unless this is on, it is not kept on this computer.")
                        .disabled(busy)
                        .on_change({
                            let sheet = cx.entity();
                            move |on, _, cx| {
                                sheet.update(cx, |this, cx| {
                                    this.remember = on;
                                    cx.notify();
                                })
                            }
                        })
                        .into_any_element(),
                    self.actions("Sign In", ready && !busy, d.busy, cx, |this, cx| this.submit(cx)),
                ]
            }
            Page::Code { phone_options } => {
                let code_ready =
                    self.code.read(cx).code().len() == rustet_app::account::lossless::CODE_LENGTH;
                let mut items = vec![
                    div()
                        .when(busy, |d| d.opacity(0.6))
                        .child(self.code.clone())
                        .into_any_element(),
                    div()
                        .flex()
                        .gap(theme.space.s)
                        .child(
                            Button::new("resend", "Resend code")
                                .disabled(busy)
                                .on_click(
                                    cx.listener(|_, _, _, cx| cx.emit(SignInRequest::ResendCode)),
                                ),
                        )
                        .child(
                            Button::new("cant-get", "Can’t get to your devices?")
                                .disabled(busy)
                                .on_click(cx.listener(|_, _, _, cx| {
                                    cx.emit(SignInRequest::TogglePhoneOptions)
                                })),
                        )
                        .into_any_element(),
                ];
                if *phone_options {
                    items.push(
                        div()
                            .flex()
                            .flex_col()
                            .items_start()
                            .gap(theme.space.s)
                            .p(theme.space.m)
                            .rounded(theme.radius.menu)
                            .bg(colors.fill)
                            .child(note(
                                "Apple can send the code to your phone number instead.",
                            ))
                            .child(
                                Button::new("text-me", "Text me a verification code")
                                    .primary()
                                    .on_click(cx.listener(|_, _, _, cx| {
                                        cx.emit(SignInRequest::TextMeACode)
                                    })),
                            )
                            .into_any_element(),
                    );
                }
                items.push(
                    note("No code? Apple shows it on your other devices signed in to this Apple Account. On an iPhone or iPad it is also in Settings, your name, Sign-In & Security.")
                        .into_any_element(),
                );
                items.push(self.actions(
                    "Continue",
                    code_ready && !busy,
                    d.busy,
                    cx,
                    |this, cx| {
                        let code = this.code.read(cx).code().to_owned();
                        this.submit_code(code, cx)
                    },
                ));
                items
            }
            Page::Choice { options } => {
                let mut items: Vec<AnyElement> = options
                    .iter()
                    .enumerate()
                    .map(|(i, label)| {
                        Button::new(("choice", i), label.clone())
                            .large()
                            .full_width()
                            .disabled(busy)
                            .on_click(
                                cx.listener(move |_, _, _, cx| cx.emit(SignInRequest::Choose(i))),
                            )
                            .into_any_element()
                    })
                    .collect();
                items.push(
                    div()
                        .flex()
                        .justify_end()
                        .gap(theme.space.s)
                        .children(d.busy.map(note))
                        .child(
                            Button::new("sign-in-cancel", "Cancel")
                                .large()
                                .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
                        )
                        .into_any_element(),
                );
                items
            }
        };
        if let Some(error) = &d.error {
            sheet = sheet.child(error_line(error, cx));
        }
        sheet.children(body).into_any_element()
    }
}

impl SignInSheet {
    /// Cancel and the primary answer, with what the sheet waits for.
    fn actions(
        &self,
        label: &'static str,
        enabled: bool,
        busy: Option<&'static str>,
        cx: &mut Context<Self>,
        f: fn(&mut Self, &mut Context<Self>),
    ) -> AnyElement {
        let theme = cx.theme().clone();
        div()
            .flex()
            .items_center()
            .justify_end()
            .gap(theme.space.s)
            .pt(theme.space.s)
            .children(busy.map(|b| {
                div()
                    .id("sign-in-busy")
                    .role(Role::Status)
                    .aria_label(b)
                    .flex_1()
                    .text_size(theme.text.callout)
                    .text_color(theme.colors.text_secondary)
                    .child(b)
            }))
            .child(
                Button::new("sign-in-cancel", "Cancel")
                    .large()
                    .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
            )
            .child(
                Button::new("sign-in-primary", label)
                    .primary()
                    .large()
                    .disabled(!enabled)
                    .on_click(cx.listener(move |this, _, _, cx| f(this, cx))),
            )
            .into_any_element()
    }
}

/// The note while the web sign-in window is open (an engine without FairPlay).
pub fn web_window_sheet(account: AccountModel, cx: &mut App) -> AnyElement {
    let theme = cx.theme().clone();
    let cancel = account.clone();
    Sheet::new("web-sign-in", "Sign in to Apple Music")
        .on_dismiss(move |_, _| cancel.web().cancel_sign_in())
        .child(
            div()
                .text_size(theme.text.body)
                .line_height(theme.text.body_line)
                .text_color(theme.colors.text_secondary)
                .child("Finish signing in in the Apple Music window. Rustet continues by itself when you are signed in."),
        )
        .child(
            div().flex().justify_end().pt(theme.space.s).child(
                Button::new("web-sign-in-cancel", "Cancel")
                    .large()
                    .on_click(move |_, _, _| account.web().cancel_sign_in()),
            ),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_sheet_state_has_a_drawing() {
        assert_eq!(sheet_drawing(&SheetView::Closed), None);

        let new = sheet_drawing(&SheetView::SignIn {
            email: String::new(),
            known_account: false,
            busy: false,
            error: None,
        })
        .unwrap();
        assert_eq!(
            new.page,
            Page::SignIn {
                email_editable: true
            }
        );
        assert_eq!(
            (new.title.as_str(), new.busy),
            ("Sign in with your Apple Account", None)
        );

        let known = sheet_drawing(&SheetView::SignIn {
            email: "a@example.com".into(),
            known_account: true,
            busy: true,
            error: None,
        })
        .unwrap();
        assert_eq!(
            known.page,
            Page::SignIn {
                email_editable: false
            },
            "only the password is asked for"
        );
        assert_eq!(known.title, "Sign-in required");
        assert!(known.message.contains("a@example.com"));
        assert_eq!(known.busy, Some("Signing in…"));

        let error = SheetError {
            message: "Your Apple Account is locked.".into(),
            help_url: Some("https://iforgot.apple.com".into()),
        };
        let failed = sheet_drawing(&SheetView::SignIn {
            email: String::new(),
            known_account: false,
            busy: false,
            error: Some(error.clone()),
        })
        .unwrap();
        assert_eq!(failed.error, Some(error), "the error and where to fix it");

        let wrong = sheet_drawing(&SheetView::Code {
            busy: None,
            error: None,
            wrong_code: true,
            phone_options: true,
        })
        .unwrap();
        assert!(wrong.clear_code);
        assert_eq!(
            wrong.page,
            Page::Code {
                phone_options: true
            }
        );
        for (wait, label) in [
            (CodeWait::Verifying, "Verifying…"),
            (CodeWait::Resending, "Resending…"),
            (CodeWait::SendingSms, "Sending SMS…"),
        ] {
            let d = sheet_drawing(&SheetView::Code {
                busy: Some(wait),
                error: None,
                wrong_code: false,
                phone_options: false,
            })
            .unwrap();
            assert_eq!(d.busy, Some(label));
        }

        let choice = sheet_drawing(&SheetView::Choice {
            title: "Recovery Options".into(),
            options: vec!["Text code".into(), "Call".into()],
            busy: false,
        })
        .unwrap();
        assert_eq!(
            choice.page,
            Page::Choice {
                options: vec!["Text code".into(), "Call".into()]
            }
        );
    }
}
