//! Line-synced lyrics over `rustet_app`'s [`Lyrics`] model.
//!
//! The active line follows the playback position; with word timings the line fills word by
//! word, otherwise it lights as a whole. The list eases to keep the active line at a third of
//! the height (jumps with Reduce animations). A click or Enter on a line asks to seek there
//! ([`LyricsEvent::Seek`]). Unsynced lyrics are shown as plain lines.
//!
//! The position: the player publishes `positionMs` at `positionAt` and may go quiet for
//! seconds, so the view interpolates while playing ([`LyricsView::set_position`]) and repaints
//! at the display rate only while playing.

use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{
    Context, EventEmitter, FocusHandle, Focusable, HighlightStyle, ListAlignment, ListOffset,
    ListState, Role, SharedString, StyledText, Window, div, list, prelude::*, px,
};
use rustet_app::pages::lyrics::{LyricLines, Lyrics, TimedLine};

use crate::components::FocusRing;
use crate::theme::{ActiveTheme, BOLD};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LyricsEvent {
    /// Seek to the start of a line.
    Seek(Duration),
}

/// Where playback is: a position at an instant, advancing while playing.
#[derive(Debug, Clone, Copy)]
struct Clock {
    position: Duration,
    at: Instant,
    playing: bool,
}

impl Clock {
    fn now(&self) -> Duration {
        if self.playing {
            self.position + self.at.elapsed()
        } else {
            self.position
        }
    }
}

pub struct LyricsView {
    lyrics: Rc<Lyrics>,
    clock: Clock,
    list: ListState,
    focus_handle: FocusHandle,
}

impl EventEmitter<LyricsEvent> for LyricsView {}

impl LyricsView {
    pub fn new(lyrics: Lyrics, cx: &mut Context<Self>) -> Self {
        let count = match &lyrics.lines {
            LyricLines::Synced(l) => l.len(),
            LyricLines::Unsynced(l) => l.len(),
        };
        Self {
            lyrics: Rc::new(lyrics),
            clock: Clock {
                position: Duration::ZERO,
                at: Instant::now(),
                playing: false,
            },
            list: ListState::new(count, ListAlignment::Top, px(400.)),
            focus_handle: cx.focus_handle(),
        }
    }

    /// The player's position at `at`, and whether it advances.
    pub fn set_position(
        &mut self,
        position: Duration,
        at: Instant,
        playing: bool,
        cx: &mut Context<Self>,
    ) {
        self.clock = Clock {
            position,
            at,
            playing,
        };
        cx.notify();
    }

    fn follow(&self, active: usize, instant: bool) {
        let viewport = self.list.viewport_bounds();
        match self.list.bounds_for_item(active) {
            Some(item) if !instant => {
                let goal = viewport.top() + viewport.size.height * 0.33;
                let delta = item.top() - goal;
                if delta.abs() > px(0.5) {
                    self.list.scroll_by(delta * 0.18);
                }
            }
            _ => self.list.scroll_to(ListOffset {
                item_ix: active.saturating_sub(2),
                offset_in_item: px(0.),
            }),
        }
    }
}

/// How much of `line` has been sung at `t`, as a byte offset into its text.
pub fn sung_bytes(line: &TimedLine, next_start: Option<Duration>, t: Duration) -> usize {
    if !line.words.is_empty() {
        // Words are in the text in order: count the bytes of the started ones.
        let mut offset = 0;
        let mut sung = 0;
        for w in &line.words {
            let Some(found) = line.text[offset..].find(w.text.as_str()) else {
                break;
            };
            let end = offset + found + w.text.len();
            if w.start <= t {
                sung = end;
            }
            offset = end;
        }
        return sung;
    }
    let end = next_start
        .unwrap_or(line.end)
        .max(line.start + Duration::from_millis(1));
    let fraction = (t.saturating_sub(line.start).as_secs_f32() / (end - line.start).as_secs_f32())
        .clamp(0., 1.);
    if fraction >= 1. { line.text.len() } else { 0 }
}

impl Render for LyricsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let t = self.clock.now();
        let active = self.lyrics.line_at(t);
        if let Some(ix) = active {
            self.follow(ix, theme.reduce_motion);
        }
        if self.clock.playing {
            window.request_animation_frame();
        }
        let lyrics = self.lyrics.clone();
        let entity = cx.entity();
        let dim = theme.colors.text_tertiary;
        div()
            .id("lyrics")
            .role(Role::List)
            .aria_label("Lyrics")
            .track_focus(&self.focus_handle)
            .size_full()
            .child(
                list(self.list.clone(), move |ix, _, cx| {
                    let theme = cx.theme().clone();
                    let colors = theme.colors;
                    let base = div()
                        .px(theme.space.xl)
                        .py(theme.space.s)
                        .max_w(theme.size.lyrics_max_width)
                        .text_size(theme.text.large_title)
                        .line_height(theme.text.large_title_line)
                        .font_weight(BOLD);
                    match &lyrics.lines {
                        LyricLines::Unsynced(lines) => base
                            .id(ix)
                            .role(Role::ListItem)
                            .aria_label(SharedString::from(lines[ix].clone()))
                            .text_color(colors.text)
                            .child(lines[ix].clone())
                            .into_any_element(),
                        LyricLines::Synced(lines) => {
                            let line = &lines[ix];
                            let is_active = Some(ix) == active;
                            let text: SharedString = line.text.clone().into();
                            let styled = if is_active && !line.words.is_empty() {
                                let sung = sung_bytes(line, lines.get(ix + 1).map(|l| l.start), t);
                                StyledText::new(text.clone()).with_highlights([
                                    (
                                        0..sung,
                                        HighlightStyle {
                                            color: Some(colors.text),
                                            ..Default::default()
                                        },
                                    ),
                                    (
                                        sung..text.len(),
                                        HighlightStyle {
                                            color: Some(dim),
                                            ..Default::default()
                                        },
                                    ),
                                ])
                            } else {
                                StyledText::new(text.clone())
                            };
                            let start = line.start;
                            let entity = entity.clone();
                            base.id(ix)
                                .role(Role::ListItem)
                                .aria_label(text)
                                .aria_selected(is_active)
                                .tab_index(0)
                                .focus_ring(cx)
                                .rounded(theme.radius.medium)
                                .cursor_pointer()
                                .hover(|s| s.bg(colors.hover))
                                .text_color(if is_active { colors.text } else { dim })
                                .on_click(move |_, _, cx| {
                                    entity.update(cx, |_, cx| cx.emit(LyricsEvent::Seek(start)))
                                })
                                .child(styled)
                                .children(line.translation.clone().map(|tr| {
                                    div()
                                        .text_size(theme.text.body)
                                        .font_weight(crate::theme::REGULAR)
                                        .text_color(colors.text_secondary)
                                        .child(tr)
                                }))
                                .into_any_element()
                        }
                    }
                })
                .size_full()
                .pt(theme.size.lyrics_lead)
                .pb(theme.size.lyrics_tail),
            )
    }
}

impl Focusable for LyricsView {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustet_app::pages::lyrics::Word;

    fn word(text: &str, ms: u64) -> Word {
        Word {
            start: Duration::from_millis(ms),
            end: Duration::from_millis(ms + 300),
            text: text.into(),
            space_after: true,
            background: false,
        }
    }

    #[test]
    fn words_fill_as_they_start() {
        let line = TimedLine {
            start: Duration::from_secs(1),
            end: Duration::from_secs(3),
            text: "night trains hum".into(),
            words: vec![word("night", 1000), word("trains", 1500), word("hum", 2200)],
            translation: None,
            transliteration: None,
            agent: None,
        };
        assert_eq!(sung_bytes(&line, None, Duration::from_millis(900)), 0);
        assert_eq!(
            sung_bytes(&line, None, Duration::from_millis(1600)),
            "night trains".len()
        );
        assert_eq!(
            sung_bytes(&line, None, Duration::from_millis(5000)),
            line.text.len()
        );
    }
}
