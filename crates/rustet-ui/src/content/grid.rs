//! A virtualised grid of tiles (library albums and artists, "see all" pages): only the rows on
//! screen are built, so 5,000 tiles cost what one screen costs. As many columns as fit, tiles
//! stretched to fill; it emits the tiles' [`ContentEvent`]s.

use std::rc::Rc;
use std::sync::Arc;

use rustet_app::pages::model::Tile;
use std::collections::HashMap;

use gpui::{
    Context, EventEmitter, FocusHandle, Pixels, Role, ScrollStrategy, UniformListScrollHandle,
    Window, canvas, div, prelude::*, px, uniform_list,
};

use super::tile::{MoveDown, MoveLeft, MoveRight, MoveUp, TileView};
use super::{ContentEvent, OnContent};
use crate::components::Scrollbar;
use crate::grid::{GridGeometry, Step, step};
use crate::theme::ActiveTheme;

pub struct TileGrid {
    tiles: Arc<[Tile]>,
    label: gpui::SharedString,
    width: Pixels,
    /// The tiles' focus handles, made as tiles are drawn (arrow keys move between them).
    focus: HashMap<usize, FocusHandle>,
    pub scroll: UniformListScrollHandle,
}

impl EventEmitter<ContentEvent> for TileGrid {}

impl TileGrid {
    /// `label` names the grid for screen readers ("Albums").
    pub fn new(label: impl Into<gpui::SharedString>, tiles: Arc<[Tile]>) -> Self {
        Self {
            tiles,
            label: label.into(),
            width: px(0.),
            focus: HashMap::new(),
            scroll: UniformListScrollHandle::new(),
        }
    }

    pub fn set_tiles(&mut self, tiles: Arc<[Tile]>, cx: &mut Context<Self>) {
        self.tiles = tiles;
        self.focus.clear();
        cx.notify();
    }

    fn handle(&mut self, ix: usize, cx: &mut Context<Self>) -> FocusHandle {
        self.focus
            .entry(ix)
            .or_insert_with(|| cx.focus_handle())
            .clone()
    }

    /// Moves the keyboard focus from the focused tile, scrolling the target into view.
    fn step(&mut self, to: Step, window: &mut Window, cx: &mut Context<Self>) {
        let columns = self.geometry(cx).columns;
        let Some(from) = self
            .focus
            .iter()
            .find(|(_, h)| h.is_focused(window))
            .map(|(ix, _)| *ix)
        else {
            return;
        };
        let Some(target) = step(from, self.tiles.len(), columns, to) else {
            return;
        };
        self.scroll
            .scroll_to_item(target / columns, ScrollStrategy::Nearest);
        let handle = self.handle(target, cx);
        window.focus(&handle, cx);
        cx.notify();
    }

    fn geometry(&self, cx: &gpui::App) -> GridGeometry {
        let theme = cx.theme();
        GridGeometry::for_width(self.width, theme.size.tile_min, theme.space.l)
    }

    pub fn tiles(&self) -> &Arc<[Tile]> {
        &self.tiles
    }
}

impl Render for TileGrid {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let side = theme.space.xl;
        let geometry = self.geometry(cx);
        let rows = if self.width > px(0.) {
            geometry.rows(self.tiles.len())
        } else {
            0
        };
        let entity = cx.entity();
        let on_event: OnContent = {
            let entity = entity.clone();
            Rc::new(move |e, _, cx| entity.update(cx, |_, cx| cx.emit(e.clone())))
        };
        let total = self.tiles.len();
        div()
            .id("tile-grid")
            .role(Role::List)
            .aria_label(self.label.clone())
            .on_action(cx.listener(|this, _: &MoveLeft, w, cx| this.step(Step::Left, w, cx)))
            .on_action(cx.listener(|this, _: &MoveRight, w, cx| this.step(Step::Right, w, cx)))
            .on_action(cx.listener(|this, _: &MoveUp, w, cx| this.step(Step::Up, w, cx)))
            .on_action(cx.listener(|this, _: &MoveDown, w, cx| this.step(Step::Down, w, cx)))
            .relative()
            .size_full()
            .child(
                uniform_list(
                    "tile-grid-rows",
                    rows,
                    cx.processor(move |this, range: std::ops::Range<usize>, window, cx| {
                        // One tab stop for the grid; the arrow keys move inside it.
                        let count = this.tiles.len();
                        let shown: Vec<FocusHandle> = range
                            .clone()
                            .flat_map(|row| geometry.row_items(row, count))
                            .map(|ix| this.handle(ix, cx))
                            .collect();
                        crate::focus::rove(&shown, window);
                        range
                            .map(|row| {
                                div()
                                    .flex()
                                    .gap(geometry.gap)
                                    .px(side)
                                    .pb(theme.space.l28)
                                    .children(geometry.row_items(row, this.tiles.len()).map(|ix| {
                                        let focus = this.handle(ix, cx);
                                        div()
                                            .id(ix)
                                            .role(Role::ListItem)
                                            .aria_position_in_set(ix + 1)
                                            .aria_size_of_set(total)
                                            .child(
                                                TileView::new(
                                                    this.tiles[ix].clone(),
                                                    ix,
                                                    geometry.card_width,
                                                )
                                                .focus_handle(focus)
                                                .on_event(on_event.clone()),
                                            )
                                    }))
                            })
                            .collect()
                    }),
                )
                .track_scroll(&self.scroll)
                .pt(theme.space.m)
                .pb(theme.size.player_clearance)
                .size_full(),
            )
            .child(Scrollbar::new(
                "tile-grid-scrollbar",
                self.scroll.0.borrow().base_handle.clone(),
            ))
            // Measures the width the grid gets, to choose the column count.
            .child(
                canvas(
                    move |bounds, window, cx| {
                        let width = bounds.size.width - side * 2.;
                        entity.update(cx, |this, cx| {
                            if this.width != width {
                                this.width = width;
                                cx.notify();
                                // Drawn with the old width: draw again now (a notify
                                // during a paint schedules no frame).
                                window.request_animation_frame();
                            }
                        });
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
    }
}
