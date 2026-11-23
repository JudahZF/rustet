//! View tests without a display: gpui's test platform runs real views, focus, key bindings,
//! actions and the input handler in memory (no compositor, no GPU). Text shaping is a no-op
//! there, so layout and pixels are checked with the headless gallery instead.
//!
//!     cargo test -p rustet-ui --features test-support
#![cfg(feature = "test-support")]

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    App, AppContext, Context, Entity, EntityInputHandler, Focusable, IntoElement, Render,
    TestAppContext, Window, div, prelude::*,
};
use rustet_app::pages::model::{Availability, Linked, TrackId, TrackKind, TrackRow};
use rustet_ui::components::{
    self, CodeInput, CodeInputEvent, ContextMenu, MenuItem, SearchField, SearchFieldEvent, Select,
    SelectEvent, Slider, SliderEvent, Suggestion, TextInput, TextInputEvent,
};
use rustet_ui::content::{ContentEvent, TrackListOptions, TrackListView, track_list};
use rustet_ui::theme::{Appearance, Theme};

fn setup(cx: &mut TestAppContext) {
    cx.update(|cx| {
        cx.set_global(Theme::new(Appearance::Dark));
        components::bind_keys(cx);
        track_list::bind_keys(cx);
    });
}

/// A window whose root shows one child view.
struct Host(gpui::AnyView);

impl Render for Host {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.0.clone())
    }
}

fn events<E: Clone + 'static, V: gpui::EventEmitter<E>>(
    entity: &Entity<V>,
    cx: &mut App,
) -> Rc<RefCell<Vec<E>>> {
    let seen = Rc::new(RefCell::new(Vec::new()));
    let sink = seen.clone();
    cx.subscribe(entity, move |_, e: &E, _| sink.borrow_mut().push(e.clone()))
        .detach();
    seen
}

#[gpui::test]
fn text_input_edits_by_grapheme_and_composes_with_an_ime(cx: &mut TestAppContext) {
    setup(cx);
    let input = cx.new(|cx| TextInput::new("Search", cx));
    let (_, cx) = cx.add_window_view(|_, _| Host(input.clone().into()));
    cx.update(|window, cx| window.focus(&input.focus_handle(cx), cx));
    let changes = cx.update(|_, cx| events::<TextInputEvent, _>(&input, cx));

    // Typed text, including a combining sequence and an emoji with a joiner.
    cx.simulate_input("Ágætis 👩‍🎤");
    assert_eq!(
        input.read_with(cx, |i, _| i.text().clone()).as_ref(),
        "Ágætis 👩‍🎤"
    );
    // Backspace removes the whole emoji cluster, not one code point.
    cx.simulate_keystrokes("backspace");
    assert_eq!(
        input.read_with(cx, |i, _| i.text().clone()).as_ref(),
        "Ágætis "
    );

    // An input method composes "はつ" (underlined, marked), then commits "初".
    cx.update(|window, cx| {
        input.update(cx, |i, cx| {
            i.replace_and_mark_text_in_range(None, "はつ", None, window, cx)
        })
    });
    assert_eq!(
        input.read_with(cx, |i, _| i.marked_range()),
        Some(9..15),
        "byte range of the composition"
    );
    cx.update(|window, cx| {
        input.update(cx, |i, cx| i.replace_text_in_range(None, "初", window, cx))
    });
    let text = input.read_with(cx, |i, _| (i.text().clone(), i.marked_range()));
    assert_eq!((text.0.as_ref(), text.1), ("Ágætis 初", None));

    // Select all and type over it; Enter submits.
    cx.simulate_keystrokes("ctrl-a");
    cx.simulate_input("love");
    cx.simulate_keystrokes("enter");
    assert_eq!(
        changes.borrow().last(),
        Some(&TextInputEvent::Submitted("love".into()))
    );
}

#[gpui::test]
fn a_password_field_masks_and_never_hands_the_text_out(cx: &mut TestAppContext) {
    setup(cx);
    let field = cx.new(|cx| TextInput::new("Password", cx).password());
    let (_, cx) = cx.add_window_view(|_, _| Host(field.clone().into()));
    cx.update(|window, cx| window.focus(&field.focus_handle(cx), cx));
    let events = cx.update(|_, cx| events::<TextInputEvent, _>(&field, cx));

    cx.simulate_input("pass wörd");
    assert_eq!(
        field.read_with(cx, |f, _| f.text().clone()).as_ref(),
        "pass wörd"
    );
    cx.simulate_keystrokes("backspace enter");
    assert_eq!(
        events.borrow().last(),
        Some(&TextInputEvent::Submitted("pass wör".into()))
    );

    // An input method gets no text and cannot compose.
    let text = cx.update(|window, cx| {
        field.update(cx, |f, cx| {
            f.replace_and_mark_text_in_range(None, "は", None, window, cx);
            f.text_for_range(0..3, &mut None, window, cx)
        })
    });
    assert_eq!(text, None);
    assert_eq!(field.read_with(cx, |f, _| f.marked_range()), None);

    // Disabled while a request runs: no typing.
    field.update(cx, |f, cx| f.set_disabled(true, cx));
    cx.simulate_input("x");
    assert_eq!(
        field.read_with(cx, |f, _| f.text().clone()).as_ref(),
        "pass wör"
    );
}

#[gpui::test]
fn code_input_takes_six_digits_and_full_width_ones(cx: &mut TestAppContext) {
    setup(cx);
    let code = cx.new(CodeInput::new);
    let (_, cx) = cx.add_window_view(|_, _| Host(code.clone().into()));
    cx.update(|window, cx| window.focus(&code.focus_handle(cx), cx));
    let done = cx.update(|_, cx| events::<CodeInputEvent, _>(&code, cx));
    cx.simulate_input("12a");
    cx.simulate_keystrokes("backspace");
    cx.simulate_input("1３4567");
    assert_eq!(*done.borrow(), [CodeInputEvent::Complete("113456".into())]);
}

fn row(i: usize) -> TrackRow {
    TrackRow {
        id: TrackId::Catalog(i.to_string()),
        kind: TrackKind::Song,
        number: Some(i as u32 + 1),
        disc: None,
        title: format!("Track {i}"),
        explicit: false,
        artist: Linked {
            text: "Artist".into(),
            route: None,
        },
        album: None,
        duration: Some(Duration::from_secs(180)),
        traits: Vec::new(),
        apple_digital_master: false,
        availability: Availability::Playable,
        artwork: None,
        popularity: None,
    }
}

#[gpui::test]
fn track_list_selects_with_keys_and_plays_on_enter(cx: &mut TestAppContext) {
    setup(cx);
    let rows: Arc<[TrackRow]> = (0..10_000).map(row).collect();
    let list = cx.new(|cx| TrackListView::new(rows, TrackListOptions::default(), cx));
    let (_, cx) = cx.add_window_view(|_, _| Host(list.clone().into()));
    cx.update(|window, cx| window.focus(&list.focus_handle(cx), cx));
    let seen = cx.update(|_, cx| events::<ContentEvent, _>(&list, cx));

    cx.simulate_keystrokes("down down down up");
    assert_eq!(list.read_with(cx, |l, _| l.selection()), [1]);
    // Shift extends from the anchor; Escape clears; Ctrl+A takes every row.
    cx.simulate_keystrokes("shift-down shift-down");
    assert_eq!(list.read_with(cx, |l, _| l.selection()), [1, 2, 3]);
    cx.simulate_keystrokes("escape ctrl-a");
    assert_eq!(list.read_with(cx, |l, _| l.selection().len()), 10_000);

    cx.simulate_keystrokes("escape end enter");
    let played: Vec<usize> = seen
        .borrow()
        .iter()
        .filter_map(|e| match e {
            ContentEvent::PlayTracks { start, .. } => Some(*start),
            _ => None,
        })
        .collect();
    assert_eq!(played, [9_999]);
}

#[gpui::test]
fn slider_steps_with_keys(cx: &mut TestAppContext) {
    setup(cx);
    let slider = cx.new(|cx| Slider::new(0.5, "Volume", cx));
    let (_, cx) = cx.add_window_view(|_, _| Host(slider.clone().into()));
    cx.update(|window, cx| window.focus(&slider.focus_handle(cx), cx));
    let seen = cx.update(|_, cx| events::<SliderEvent, _>(&slider, cx));
    cx.simulate_keystrokes("right right left");
    assert!((slider.read_with(cx, |s, _| s.value()) - 0.55).abs() < 1e-6);
    assert!(
        matches!(seen.borrow().last(), Some(SliderEvent::Committed(v)) if (v - 0.55).abs() < 1e-6)
    );
}

#[gpui::test]
fn select_opens_moves_and_chooses_by_keyboard(cx: &mut TestAppContext) {
    setup(cx);
    let options = vec![
        "High quality".into(),
        "Lossless".into(),
        "Hi-Res Lossless".into(),
    ];
    let select = cx.new(|cx| Select::new("Streaming quality", options, 0, cx));
    let (_, cx) = cx.add_window_view(|_, _| Host(select.clone().into()));
    cx.update(|window, cx| window.focus(&select.focus_handle(cx), cx));
    let seen = cx.update(|_, cx| events::<SelectEvent, _>(&select, cx));
    // Escape closes the open list without a change; Enter chooses the highlighted option.
    cx.simulate_keystrokes("enter down escape");
    assert!(seen.borrow().is_empty());
    cx.simulate_keystrokes("enter down down enter");
    assert_eq!(*seen.borrow(), [SelectEvent::Changed(2)]);
    assert_eq!(select.read_with(cx, |s, _| s.selected()), 2);
}

#[gpui::test]
fn search_field_debounces_and_drops_stale_suggestions(cx: &mut TestAppContext) {
    setup(cx);
    let field = cx.new(|cx| SearchField::new("Search", cx));
    let (_, cx) = cx.add_window_view(|_, _| Host(field.clone().into()));
    cx.update(|window, cx| window.focus(&field.focus_handle(cx), cx));
    let seen = cx.update(|_, cx| events::<SearchFieldEvent, _>(&field, cx));

    // Typing faster than the debounce asks once, for the last text.
    cx.simulate_input("ph");
    cx.executor().advance_clock(Duration::from_millis(50));
    cx.simulate_input("oebe");
    cx.executor()
        .advance_clock(components::search_field::DEBOUNCE * 2);
    cx.run_until_parked();
    assert_eq!(*seen.borrow(), [SearchFieldEvent::Query("phoebe".into())]);

    let suggestion = |t: &str| Suggestion {
        title: t.to_owned().into(),
        subtitle: None,
        artwork: None,
    };
    // A late answer for older text is dropped; the answer for the shown text opens the list.
    field.update(cx, |f, cx| {
        f.set_suggestions("ph", vec![suggestion("Phil Collins")], cx)
    });
    assert!(field.read_with(cx, |f, _| f.suggestions().is_empty()));
    field.update(cx, |f, cx| {
        f.set_suggestions(
            "phoebe",
            vec![suggestion("Phoebe Bridgers"), suggestion("Punisher")],
            cx,
        )
    });
    cx.simulate_keystrokes("down down enter");
    assert_eq!(seen.borrow().last(), Some(&SearchFieldEvent::Choose(1)));
}

#[gpui::test]
fn context_menu_runs_a_submenu_item_by_keyboard_and_closes(cx: &mut TestAppContext) {
    setup(cx);
    let (host, cx) = cx.add_window_view(|_, cx| Host(cx.new(|_| gpui::Empty).into()));
    let chosen = Rc::new(RefCell::new(Vec::new()));
    let menu = cx.update(|window, cx| {
        let (a, b) = (chosen.clone(), chosen.clone());
        let items = vec![
            MenuItem::action("Play next", move |_, _| a.borrow_mut().push("next")),
            MenuItem::Separator,
            MenuItem::action("Download", |_, _| unreachable!("disabled")).disabled(),
            MenuItem::submenu(
                "Add to playlist",
                vec![
                    MenuItem::action("Late night", |_, _| {}),
                    MenuItem::action("Running", move |_, _| b.borrow_mut().push("running")),
                ],
            ),
        ];
        ContextMenu::new(items, window, cx)
    });
    cx.update(|_, cx| {
        host.update(cx, |h, cx| {
            h.0 = menu.clone().into();
            cx.notify();
        })
    });
    let dismissed = Rc::new(RefCell::new(0));
    let count = dismissed.clone();
    cx.update(|_, cx| {
        cx.subscribe(&menu, move |_, _: &gpui::DismissEvent, _| {
            *count.borrow_mut() += 1
        })
        .detach()
    });

    // Down skips the separator and the disabled item; Right opens the sub-menu, Left closes
    // it again without closing the menu.
    cx.simulate_keystrokes("down down right left");
    assert_eq!(*dismissed.borrow(), 0);
    cx.simulate_keystrokes("right down enter");
    assert_eq!(*chosen.borrow(), ["running"]);
    assert_eq!(*dismissed.borrow(), 1);
}

// ── Music video ─────────────────────────────────────────────────────────────

mod video {
    use std::time::Duration;

    use gpui::{AppContext, Entity, TestAppContext, VisualTestContext};
    use rustet_framering::{Format, Writer, now_us};
    use rustet_player::model::VideoSurface;
    use rustet_ui::video::{Phase, VideoSource, VideoView};

    use super::{Host, setup};

    fn ring(dir: &std::path::Path, generation: u64, width: u32, height: u32) -> Writer {
        Writer::create(dir, generation, Format::bgra(width, height)).unwrap()
    }

    fn source(w: &Writer, video: u64) -> Option<VideoSource> {
        let f = w.format();
        Some(VideoSource {
            surface: VideoSurface {
                ring: w.path().to_string_lossy().into_owned(),
                generation: w.generation(),
                width: f.width,
                height: f.height,
            },
            video,
        })
    }

    fn write(w: &mut Writer) {
        let f = w.format();
        w.write_frame(&vec![0x80; f.frame_bytes()], now_us())
            .unwrap();
    }

    /// Lets the reader poll and the view take what it found.
    fn tick(cx: &mut VisualTestContext) {
        for _ in 0..5 {
            cx.executor().advance_clock(Duration::from_millis(10));
            cx.run_until_parked();
        }
    }

    fn view(cx: &mut TestAppContext) -> (Entity<VideoView>, &mut VisualTestContext) {
        setup(cx);
        let (host, cx) = cx.add_window_view(|window, cx| {
            let video = cx.new(|cx| VideoView::new(window, cx));
            Host(video.into())
        });
        let video = host.read_with(cx, |h, _| h.0.clone().downcast::<VideoView>().unwrap());
        (video, cx)
    }

    #[gpui::test]
    fn a_surface_shows_its_newest_frame_and_holds_it_while_paused(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (video, cx) = view(cx);
        let mut w = ring(dir.path(), 1, 64, 36);
        video.update(cx, |v, cx| v.set_source(source(&w, 7), cx));
        tick(cx);
        assert_eq!(video.read_with(cx, |v, _| v.phase()), Phase::Waiting);

        write(&mut w);
        write(&mut w);
        tick(cx);
        let (phase, size, stats) =
            video.read_with(cx, |v, _| (v.phase(), v.frame_size(), v.stats()));
        assert_eq!((phase, size), (Phase::Playing, Some((64, 36))));
        assert_eq!(stats.last_number, 2, "the newest frame");

        // Paused: no new frame, nothing new presented, the frame stays.
        tick(cx);
        assert_eq!(video.read_with(cx, |v, _| v.stats()), stats);
        assert_eq!(video.read_with(cx, |v, _| v.frame_size()), Some((64, 36)));
    }

    #[gpui::test]
    fn a_new_ring_of_the_same_video_keeps_the_frame_until_its_own(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (video, cx) = view(cx);
        let mut first = ring(dir.path(), 1, 64, 36);
        video.update(cx, |v, cx| v.set_source(source(&first, 7), cx));
        write(&mut first);
        tick(cx);

        // A seek or a new size: the engine makes a new ring.
        let mut second = ring(dir.path(), 2, 32, 18);
        first.supersede();
        video.update(cx, |v, cx| v.set_source(source(&second, 7), cx));
        tick(cx);
        assert_eq!(
            video.read_with(cx, |v, _| v.frame_size()),
            Some((64, 36)),
            "held"
        );
        write(&mut second);
        tick(cx);
        assert_eq!(video.read_with(cx, |v, _| v.frame_size()), Some((32, 18)));

        // The ring ends: the last frame stays until the state says otherwise.
        drop(second);
        tick(cx);
        let (phase, size) = video.read_with(cx, |v, _| (v.phase(), v.frame_size()));
        assert_eq!((phase, size), (Phase::Held, Some((32, 18))));

        // Another video starts clean.
        let third = ring(dir.path(), 3, 64, 36);
        video.update(cx, |v, cx| v.set_source(source(&third, 8), cx));
        tick(cx);
        let (phase, size) = video.read_with(cx, |v, _| (v.phase(), v.frame_size()));
        assert_eq!((phase, size), (Phase::Waiting, None));

        video.update(cx, |v, cx| v.set_source(None, cx));
        assert_eq!(video.read_with(cx, |v, _| v.phase()), Phase::Idle);
    }
}
