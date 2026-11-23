//! A confirmation ("Clear the song cache?") handed to the window as a modal.

use std::rc::Rc;

use gpui::{App, AppContext as _, Context, IntoElement, Render, SharedString, Window};
use rustet_ui::components::Dialog;
use rustet_ui::host::{Host, HostRequest};

type Confirmed = Rc<dyn Fn(&mut Window, &mut App)>;

pub struct Confirm {
    title: SharedString,
    message: SharedString,
    action: SharedString,
    on_confirm: Confirmed,
}

/// Asks `title`; `action` ("Clear") runs `on_confirm`, Cancel, Escape or a click outside do
/// nothing. The window closes the modal and gives the focus back.
pub fn confirm(
    title: impl Into<SharedString>,
    message: impl Into<SharedString>,
    action: impl Into<SharedString>,
    on_confirm: impl Fn(&mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    let view = cx.new(|_| Confirm {
        title: title.into(),
        message: message.into(),
        action: action.into(),
        on_confirm: Rc::new(on_confirm),
    });
    Host::request(HostRequest::Modal(view.into()), window, cx);
}

fn close(window: &mut Window, cx: &mut App) {
    Host::request(HostRequest::CloseModal, window, cx);
}

impl Render for Confirm {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let on_confirm = self.on_confirm.clone();
        Dialog::new("confirm", self.title.clone(), self.message.clone())
            .answer(self.action.clone(), move |window, cx| {
                close(window, cx);
                on_confirm(window, cx);
            })
            .answer("Cancel", close)
            .on_dismiss(close)
    }
}
