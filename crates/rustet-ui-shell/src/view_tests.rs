//! View tests on gpui's test platform (no display): `cargo test -p rustet-ui-shell --features
//! test-support`. The logic of the drawings is tested beside it; these drive the views.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{
    AnyView, Context, Entity, IntoElement, Render, TestAppContext, Window, div, prelude::*,
};
use rustet_player::model::{Command, PlayerState, Seek, Status};
use rustet_ui::components::{Slider, SliderEvent};
use rustet_ui::theme::{Appearance, Theme};

use gpui::Focusable;
use rustet_app::account::lossless::SheetView;
use rustet_ui::components::TextInput;

use crate::deck::Scrubber;
use crate::signin::{SignInRequest, SignInSheet};

struct Host(AnyView);

impl Render for Host {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.0.clone())
    }
}

fn setup(cx: &mut TestAppContext) {
    cx.update(|cx| {
        cx.set_global(Theme::new(Appearance::Dark));
        rustet_ui::components::bind_keys(cx);
        rustet_ui::focus::bind_keys(cx);
        crate::shell::bind_keys(cx);
    });
}

fn playing() -> PlayerState {
    PlayerState {
        generation: 9,
        status: Status::Playing,
        track: Some(rustet_player::model::QueueItem {
            uid: 1,
            id: rustet_player::model::CatalogId("1".into()),
            library_id: None,
            kind: rustet_player::model::ItemKind::Song,
            source: rustet_player::model::ItemSource::User,
            meta: rustet_player::model::TrackMeta {
                name: "Kyoto".into(),
                artist_name: "Phoebe Bridgers".into(),
                album_name: "Punisher".into(),
                artwork_url: String::new(),
                duration_ms: 200_000,
                audio_traits: vec![],
                content_rating: None,
            },
            container_id: None,
        }),
        position_ms: 50_000,
        position_at: rustet_app::player::unix_ms(),
        duration_ms: 200_000,
        ..PlayerState::default()
    }
}

/// The slider inside the scrubber.
fn seek_slider(scrubber: &Entity<Scrubber>, cx: &mut gpui::VisualTestContext) -> Entity<Slider> {
    scrubber.read_with(cx, |s, _| s.seek_slider())
}

#[gpui::test]
fn the_scrubber_follows_the_player_and_seeks_with_the_generation(cx: &mut TestAppContext) {
    setup(cx);
    let scrubber = cx.new(Scrubber::new);
    let (_, cx) = cx.add_window_view(|_, _| Host(scrubber.clone().into()));
    let commands = Rc::new(RefCell::new(Vec::new()));
    let sink = commands.clone();
    cx.update(|_, cx| {
        cx.subscribe(&scrubber, move |_, c: &Command, _| {
            sink.borrow_mut().push(c.clone())
        })
        .detach();
    });

    scrubber.update(cx, |s, cx| s.set_state(Some(playing()), cx));
    let slider = seek_slider(&scrubber, cx);
    let value = slider.read_with(cx, |s, _| s.value());
    assert!(
        (0.24..0.27).contains(&value),
        "50 s of 200 s, and a moment more: {value}"
    );

    // A drag ends: one seek, for the track that was shown.
    slider.update(cx, |_, cx| cx.emit(SliderEvent::Committed(0.5)));
    cx.run_until_parked();
    assert_eq!(
        commands.borrow().as_slice(),
        [Command::Seek(Seek {
            position_ms: 100_000,
            expect_generation: Some(9),
        })]
    );

    // Nothing to seek in while a track opens.
    let mut opening = playing();
    opening.status = Status::Opening;
    scrubber.update(cx, |s, cx| s.set_state(Some(opening), cx));
    slider.update(cx, |_, cx| cx.emit(SliderEvent::Committed(0.9)));
    cx.run_until_parked();
    assert_eq!(commands.borrow().len(), 1);
}

gpui::actions!(test_host, [WindowEscape]);

/// A window like the shell's around the sign-in sheet: one tab stop before it (the sheet must
/// keep the focus) and its own Escape (the sheet's must come first).
struct SheetHost {
    outside: gpui::FocusHandle,
    sheet: Entity<SignInSheet>,
    window_escapes: usize,
}

impl Render for SheetHost {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        rustet_ui::focus::traverse(div())
            .key_context("TestHost")
            .on_action(cx.listener(|this, _: &WindowEscape, _, _| this.window_escapes += 1))
            .size_full()
            .child(
                div()
                    .id("outside")
                    .track_focus(&self.outside)
                    .child("outside"),
            )
            .child(self.sheet.clone())
    }
}

/// The sign-in sheet's email page in a window, with the requests it makes.
fn sign_in_sheet(
    cx: &mut TestAppContext,
) -> (
    Entity<SheetHost>,
    Rc<RefCell<Vec<SignInRequest>>>,
    &mut gpui::VisualTestContext,
) {
    setup(cx);
    let sheet = cx.new(SignInSheet::new);
    let requests = Rc::new(RefCell::new(Vec::new()));
    let sink = requests.clone();
    cx.update(|cx| {
        cx.subscribe(&sheet, move |_, r: &SignInRequest, _| {
            sink.borrow_mut().push(r.clone())
        })
        .detach();
    });
    cx.update(|cx| {
        cx.bind_keys([gpui::KeyBinding::new(
            "escape",
            WindowEscape,
            Some("TestHost"),
        )])
    });
    let (host, cx) = cx.add_window_view(|_, cx| SheetHost {
        outside: cx.focus_handle().tab_stop(true),
        sheet: sheet.clone(),
        window_escapes: 0,
    });
    sheet.update(cx, |s, cx| {
        s.set_view(
            SheetView::SignIn {
                email: String::new(),
                known_account: false,
                busy: false,
                error: None,
            },
            cx,
        )
    });
    cx.run_until_parked();
    (host, requests, cx)
}

fn fields(
    host: &Entity<SheetHost>,
    cx: &mut gpui::VisualTestContext,
) -> (Entity<TextInput>, Entity<TextInput>) {
    host.read_with(cx, |h, cx| {
        let s = h.sheet.read(cx);
        (s.email.clone(), s.password.clone())
    })
}

fn has_focus(input: &Entity<TextInput>, cx: &mut gpui::VisualTestContext) -> bool {
    cx.update(|window, cx| input.focus_handle(cx).is_focused(window))
}

#[gpui::test]
fn tab_moves_from_email_to_password_and_back(cx: &mut TestAppContext) {
    let (host, _, cx) = sign_in_sheet(cx);
    let (email, password) = fields(&host, cx);
    assert!(has_focus(&email, cx), "the empty email field has the focus");

    cx.simulate_input("someone@example.com");
    cx.simulate_keystrokes("tab");
    assert!(has_focus(&password, cx));
    cx.simulate_input("not-a-password");
    assert_eq!(
        email.read_with(cx, |e, _| e.text().to_string()),
        "someone@example.com"
    );
    assert_eq!(
        password.read_with(cx, |p, _| p.text().to_string()),
        "not-a-password"
    );
    assert!(
        password.read_with(cx, |p, _| p.is_masked()),
        "typed after Tab, the password is masked"
    );

    cx.simulate_keystrokes("shift-tab");
    assert!(has_focus(&email, cx));
}

#[gpui::test]
fn tab_stays_inside_the_sign_in_sheet(cx: &mut TestAppContext) {
    let (host, _, cx) = sign_in_sheet(cx);
    let (email, _) = fields(&host, cx);
    let outside = host.read_with(cx, |h, _| h.outside.clone());
    cx.simulate_input("someone@example.com");
    cx.simulate_keystrokes("tab");
    cx.simulate_input("not-a-password");
    // Email, password, Show, the remember toggle, Cancel, Sign in: then back to the email.
    let mut steps = 0;
    loop {
        cx.simulate_keystrokes("tab");
        steps += 1;
        assert!(
            !cx.update(|window, _| outside.is_focused(window)),
            "the focus left the sheet"
        );
        if has_focus(&email, cx) {
            break;
        }
        assert!(steps < 10, "Tab never wrapped to the email field");
    }
    assert_eq!(steps, 5, "Tab visits each of the sheet's six controls once");
    cx.simulate_keystrokes("shift-tab");
    assert!(!cx.update(|window, _| outside.is_focused(window)));
    assert!(
        !has_focus(&email, cx),
        "Shift+Tab from the first control wraps to the last"
    );
}

#[gpui::test]
fn enter_in_the_email_field_moves_on_then_submits(cx: &mut TestAppContext) {
    let (host, requests, cx) = sign_in_sheet(cx);
    let (email, password) = fields(&host, cx);
    cx.simulate_input("someone@example.com");
    cx.simulate_keystrokes("enter");
    assert!(
        has_focus(&password, cx),
        "Enter with no password moves to the password"
    );
    assert!(requests.borrow().is_empty());

    cx.simulate_input("not-a-password");
    cx.simulate_keystrokes("shift-tab");
    assert!(has_focus(&email, cx));
    cx.simulate_keystrokes("enter");
    assert_eq!(
        *requests.borrow(),
        [SignInRequest::SignIn {
            email: "someone@example.com".into(),
            password: "not-a-password".into(),
            remember: false,
        }]
    );
}

#[gpui::test]
fn escape_cancels_the_sign_in_sheet_from_a_filled_field(cx: &mut TestAppContext) {
    let (host, requests, cx) = sign_in_sheet(cx);
    let (email, _) = fields(&host, cx);
    cx.simulate_input("someone@example.com");
    cx.simulate_keystrokes("escape");
    assert_eq!(*requests.borrow(), [SignInRequest::Cancel]);
    assert_eq!(
        host.read_with(cx, |h, _| h.window_escapes),
        0,
        "the sheet's Escape comes first"
    );
    assert_eq!(
        email.read_with(cx, |e, _| e.text().to_string()),
        "someone@example.com"
    );
}
