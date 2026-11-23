//! The Downloads page (spec §1.5): the engine's export jobs from `App::downloads()`, with
//! cancel, retry, priority, clear finished and the folder. Jobs are rows in the track-list
//! style under two heads, "Downloading" and "Finished"; a row's actions are in its "more"
//! menu (`docs/native-ui-look.md`, "Settings and Downloads").

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use gpui::{
    AnyElement, Context, IntoElement, MouseButton, ParentElement, Pixels, Point, Render, Role,
    ScrollHandle, SharedString, Styled, Task, Window, div, prelude::*, relative,
};
use rustet_app::App;
use rustet_app::downloads::{DownloadError, DownloadList};
use rustet_app::engine::types::{ExportJob, ExportPhase};
use rustet_app::pages::model::Artwork;
use rustet_app::pages::model::EmptyState;
use rustet_ui::components::{Button, EmptyStateView, IconButton, MenuItem, Scrollbar, Toast};
use rustet_ui::content::ArtworkView;
use rustet_ui::content::artwork::Shape;
use rustet_ui::host::{Host, HostRequest};
use rustet_ui::icons::Icon;
use rustet_ui::theme::{ActiveTheme, BOLD};

use crate::format;
use crate::ui::{self, follow, run_in};

/// What a job row says about its job.
#[derive(Debug, Clone, PartialEq)]
pub enum JobStatus {
    /// `place`: 1-based run order, 0 when unknown.
    Queued {
        place: usize,
    },
    Running {
        percent: u32,
    },
    Done,
    Failed,
    Cancelled,
}

/// What a row offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobAction {
    /// Run this queued job next.
    DownloadNext,
    Cancel,
    Retry,
    /// Remove a finished job from the list.
    Remove,
    /// Show the saved file in the file manager.
    ShowFile,
}

impl JobAction {
    pub const fn label(self) -> &'static str {
        match self {
            Self::DownloadNext => "Download next",
            Self::Cancel => "Cancel",
            Self::Retry => "Try again",
            Self::Remove => "Remove from list",
            Self::ShowFile => "Show in folder",
        }
    }
}

/// One job, as its row draws it.
#[derive(Debug, Clone, PartialEq)]
pub struct JobRow {
    pub id: String,
    pub title: String,
    pub artist: String,
    /// The engine's artwork URL; `None` when it has none.
    pub artwork: Option<String>,
    pub status: JobStatus,
    /// The line under the title: the stage, progress and speed, where it was saved, or why it
    /// failed.
    pub line: String,
    /// 0–1 while running.
    pub progress: Option<f32>,
    pub actions: Vec<JobAction>,
}

/// `~` for the home folder, as people write paths.
fn tilde(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && path.starts_with(&home) => {
            format!("~{}", &path[home.len()..])
        }
        _ => path.to_owned(),
    }
}

impl JobStatus {
    /// The short state at the right of a row.
    pub fn label(&self) -> String {
        match self {
            Self::Queued { .. } => "Queued".to_owned(),
            Self::Running { percent } => format!("{percent} %"),
            Self::Done => "Done".to_owned(),
            Self::Failed => "Failed".to_owned(),
            Self::Cancelled => "Cancelled".to_owned(),
        }
    }

    /// Still to finish: running or queued.
    pub const fn active(&self) -> bool {
        matches!(self, Self::Queued { .. } | Self::Running { .. })
    }
}

impl JobRow {
    /// `speed`: bytes per second measured from the job's progress, while downloading.
    pub fn new(job: &ExportJob, speed: Option<f64>) -> Self {
        use JobAction as A;
        let (status, line, actions) = match job.phase {
            ExportPhase::Queued => (
                JobStatus::Queued {
                    place: job.queue_index,
                },
                match job.queue_index {
                    0 => "Queued".to_owned(),
                    1 => "Queued, next".to_owned(),
                    n => format!("Queued, {} in line", format::ordinal(n)),
                },
                vec![A::DownloadNext, A::Cancel],
            ),
            ExportPhase::Done => (
                JobStatus::Done,
                if job.output.is_empty() {
                    "Downloaded".to_owned()
                } else {
                    format!("Saved to {}", tilde(&job.output))
                },
                if job.output.is_empty() {
                    vec![A::Remove]
                } else {
                    vec![A::ShowFile, A::Remove]
                },
            ),
            ExportPhase::Failed => (
                JobStatus::Failed,
                if job.error.is_empty() {
                    "The download failed. Try again.".to_owned()
                } else {
                    job.error.clone()
                },
                vec![A::Retry, A::Remove],
            ),
            ExportPhase::Cancelled => (
                JobStatus::Cancelled,
                "Cancelled".to_owned(),
                vec![A::Retry, A::Remove],
            ),
            phase => {
                let stage = match phase {
                    ExportPhase::Resolving => "Preparing".to_owned(),
                    ExportPhase::Downloading => {
                        let mut s = format!("Downloading, {} %", job.percent.min(100));
                        if job.bytes_total > 0 {
                            s += &format!(" of {}", format::bytes(job.bytes_total));
                        }
                        if let Some(bps) = speed.filter(|s| *s > 0.) {
                            s += &format!(", {}/s", format::bytes(bps as u64));
                        }
                        match (job.source.as_deref(), job.throttled, job.limit_bps) {
                            (Some("cache"), _, _) => s += ", from the song cache",
                            (_, true, Some(limit)) if limit > 0 => {
                                s += &format!(
                                    ", limited to {}/s while music plays",
                                    format::bytes(limit as u64)
                                );
                            }
                            _ => {}
                        }
                        s
                    }
                    ExportPhase::Tagging => "Adding tags and artwork".to_owned(),
                    ExportPhase::Moving => "Saving".to_owned(),
                    _ => "Working".to_owned(),
                };
                (
                    JobStatus::Running {
                        percent: job.percent.min(100),
                    },
                    stage,
                    vec![A::Cancel],
                )
            }
        };
        Self {
            id: job.job_id.clone(),
            title: if job.title.is_empty() {
                job.asset_id.clone()
            } else {
                job.title.clone()
            },
            artist: job.artist_name.clone(),
            artwork: (!job.artwork_url.is_empty()).then(|| job.artwork_url.clone()),
            progress: matches!(status, JobStatus::Running { .. })
                .then(|| job.percent.min(100) as f32 / 100.),
            status,
            line,
            actions,
        }
    }
}

/// The folder downloads go to: the setting, else the engine's default.
fn folder(app: &App) -> PathBuf {
    let set = app.settings().get().downloads.folder;
    if !set.is_empty() {
        return PathBuf::from(shell_home(&set));
    }
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join("Music/Rustet-Downloads")
}

fn shell_home(path: &str) -> String {
    match (path.strip_prefix("~/"), std::env::var("HOME")) {
        (Some(rest), Ok(home)) => format!("{home}/{rest}"),
        _ => path.to_owned(),
    }
}

/// The last byte count of a downloading job, for its speed.
struct Sample {
    bytes: u64,
    at: Instant,
    speed: Option<f64>,
}

pub struct DownloadsPage {
    app: App,
    list: DownloadList,
    samples: HashMap<String, Sample>,
    frozen: bool,
    scroll: ScrollHandle,
    _tasks: Vec<Task<()>>,
}

impl DownloadsPage {
    pub fn new(app: App, _: &mut Window, cx: &mut Context<Self>) -> Self {
        let tasks = vec![follow(
            app.downloads().list(),
            cx,
            |this: &mut Self, list, _| {
                if !this.frozen {
                    this.set_list(list, Instant::now());
                }
            },
        )];
        Self {
            app,
            list: DownloadList::default(),
            samples: HashMap::new(),
            frozen: false,
            scroll: ScrollHandle::new(),
            _tasks: tasks,
        }
    }

    fn set_list(&mut self, list: DownloadList, now: Instant) {
        self.samples
            .retain(|id, _| list.jobs.iter().any(|j| &j.job_id == id));
        for job in list
            .jobs
            .iter()
            .filter(|j| j.phase == ExportPhase::Downloading)
        {
            let sample = self.samples.entry(job.job_id.clone()).or_insert(Sample {
                bytes: job.bytes_done,
                at: now,
                speed: None,
            });
            let dt = now.duration_since(sample.at).as_secs_f64();
            if dt >= 0.5 && job.bytes_done >= sample.bytes {
                sample.speed = Some((job.bytes_done - sample.bytes) as f64 / dt);
                sample.bytes = job.bytes_done;
                sample.at = now;
            }
        }
        self.list = list;
    }

    fn act(&mut self, id: String, action: JobAction, window: &mut Window, cx: &mut Context<Self>) {
        let downloads = self.app.downloads().clone();
        let job = self.list.jobs.iter().find(|j| j.job_id == id).cloned();
        match action {
            JobAction::ShowFile => {
                if let Some(job) = job {
                    cx.reveal_path(std::path::Path::new(&job.output));
                }
            }
            JobAction::DownloadNext => {
                let top = self.list.jobs.iter().map(|j| j.priority).max().unwrap_or(0);
                let priority = top.saturating_add(1);
                run_in(
                    self.app.runtime(),
                    async move { downloads.set_priority(&id, priority).await },
                    window,
                    cx,
                    report,
                );
            }
            JobAction::Cancel | JobAction::Remove => {
                run_in(
                    self.app.runtime(),
                    async move { downloads.cancel(&id).await },
                    window,
                    cx,
                    report,
                );
            }
            JobAction::Retry => {
                run_in(
                    self.app.runtime(),
                    async move { downloads.retry(&id).await.map(drop) },
                    window,
                    cx,
                    report,
                );
            }
        }
    }

    fn open_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dir = folder(&self.app);
        if dir.is_dir() {
            cx.open_with_system(&dir);
        } else {
            Host::request(
                HostRequest::Toast(Toast::new(format!(
                    "{} does not exist yet. Rustet makes it with the first download.",
                    tilde(&dir.to_string_lossy())
                ))),
                window,
                cx,
            );
        }
    }

    /// The row's actions as a menu at `position` (its "more" glyph, or a right click).
    fn menu(
        &self,
        row: &JobRow,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let page = cx.entity();
        let items = row
            .actions
            .iter()
            .map(|&action| {
                let (page, id) = (page.clone(), row.id.clone());
                let icon = match action {
                    JobAction::DownloadNext => Icon::Download,
                    JobAction::Cancel | JobAction::Remove => Icon::Close,
                    JobAction::Retry => Icon::Repeat,
                    JobAction::ShowFile => Icon::Folder,
                };
                MenuItem::action(action.label(), move |window, cx| {
                    page.update(cx, |p, cx| p.act(id.clone(), action, window, cx))
                })
                .icon(icon)
            })
            .collect();
        Host::request(HostRequest::Menu { items, position }, window, cx);
    }

    /// A job in the track-list style: a 46 px row under a hairline, artwork, title and a
    /// secondary line (with a thin progress line while it runs), the state, a "more" glyph.
    fn row(&self, row: JobRow, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let colors = theme.colors;
        let failed = row.status == JobStatus::Failed;
        let name: SharedString = if row.artist.is_empty() {
            format!("{}, {}", row.title, row.line).into()
        } else {
            format!("{}, {}, {}", row.title, row.artist, row.line).into()
        };
        let secondary = if row.artist.is_empty() {
            row.line.clone()
        } else {
            format!("{} · {}", row.artist, row.line)
        };
        let artwork = row.artwork.clone().map(|template| Artwork {
            template,
            width: None,
            height: None,
            palette: None,
        });
        let page = cx.entity();
        let menu_row = row.clone();
        let more = {
            let (page, row) = (page.clone(), row.clone());
            IconButton::new(
                SharedString::from(format!("{}-more", row.id)),
                Icon::More,
                "More",
            )
            .on_click(move |e, window, cx| {
                cx.stop_propagation();
                page.update(cx, |p, cx| p.menu(&row, e.position(), window, cx))
            })
        };
        div()
            .id(SharedString::from(row.id.clone()))
            .role(Role::ListItem)
            .aria_label(name)
            .when_some(row.progress, |d, p| {
                d.aria_numeric_value(f64::from(p) * 100.)
            })
            .relative()
            .flex()
            .items_center()
            .gap(theme.space.m)
            .h(theme.size.row)
            .px(theme.space.s)
            .rounded(theme.radius.medium)
            .hover(|d| d.bg(colors.hover))
            .on_mouse_down(MouseButton::Right, move |e, window, cx| {
                page.update(cx, |p, cx| p.menu(&menu_row, e.position, window, cx))
            })
            // The hairline above the row, as a track list draws it.
            .child(
                div()
                    .absolute()
                    .top_0()
                    .left(theme.space.s)
                    .right_0()
                    .h(theme.size.hairline)
                    .bg(colors.divider),
            )
            .child(ArtworkView::new(artwork, theme.size.art_row).shape(Shape::Small))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .truncate()
                            .text_size(theme.text.body)
                            .text_color(colors.text)
                            .child(row.title.clone()),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_size(theme.text.callout)
                            .font_features(theme.tabular())
                            .text_color(colors.text_secondary)
                            .child(secondary),
                    )
                    .children(row.progress.map(|p| {
                        div()
                            .mt(theme.space.xxs)
                            .h(theme.size.progress)
                            .w_full()
                            .rounded_full()
                            .bg(colors.fill)
                            .child(div().h_full().w(relative(p)).rounded_full().bg(colors.text))
                    })),
            )
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(theme.space.xs)
                    .text_size(theme.text.body)
                    .font_features(theme.tabular())
                    .text_color(if failed {
                        colors.text
                    } else {
                        colors.text_secondary
                    })
                    .when(failed, |d| {
                        d.child(
                            Icon::Alert
                                .element()
                                .size(theme.size.icon_small)
                                .text_color(colors.text),
                        )
                    })
                    .child(row.status.label()),
            )
            .child(more)
            .into_any_element()
    }

    /// A section of the list: its 17 / 700 head (with `action` at the right) over its rows.
    fn section(
        &self,
        title: &'static str,
        action: Option<AnyElement>,
        rows: Vec<JobRow>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if rows.is_empty() {
            return None;
        }
        let theme = cx.theme().clone();
        Some(
            div()
                .flex()
                .flex_col()
                .gap(theme.space.s)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap(theme.space.m)
                        .min_h(theme.size.control)
                        .child(
                            div()
                                .text_size(theme.text.title2)
                                .font_weight(BOLD)
                                .text_color(theme.colors.text)
                                .child(title),
                        )
                        .children(action),
                )
                .child(
                    div()
                        .id(title)
                        .role(Role::List)
                        .aria_label(title)
                        .flex()
                        .flex_col()
                        .children(rows.into_iter().map(|r| self.row(r, cx))),
                )
                .into_any_element(),
        )
    }
}

/// A failed action as a toast.
fn report(
    _: &mut DownloadsPage,
    result: Result<(), DownloadError>,
    window: &mut Window,
    cx: &mut Context<DownloadsPage>,
) {
    if let Err(e) = result {
        Host::request(
            HostRequest::Toast(Toast::failure(e.to_string())),
            window,
            cx,
        );
    }
}

impl Render for DownloadsPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let page = cx.entity();
        // The title row: the title, and at its right the folder downloads go to.
        let header = div()
            .flex()
            .flex_wrap()
            .items_center()
            .justify_between()
            .gap(theme.space.m)
            .child(ui::page_title("Downloads", cx))
            .child(Button::new("open-folder", "Open folder").on_click({
                let page = page.clone();
                move |_, window, cx| page.update(cx, |p, cx| p.open_folder(window, cx))
            }));
        let speeds: HashMap<&str, f64> = self
            .samples
            .iter()
            .filter_map(|(id, s)| Some((id.as_str(), s.speed?)))
            .collect();
        let (active, finished): (Vec<JobRow>, Vec<JobRow>) = self
            .list
            .jobs
            .iter()
            .map(|j| JobRow::new(j, speeds.get(j.job_id.as_str()).copied()))
            .partition(|r| r.status.active());
        let body = if active.is_empty() && finished.is_empty() {
            let place = tilde(&folder(&self.app).to_string_lossy());
            vec![
                EmptyStateView::new(&EmptyState {
                    title: "No downloads".to_owned(),
                    message: format!(
                        "To download a song, album or playlist, open its menu and choose Download. Files are saved in {place}."
                    ),
                })
                .into_any_element(),
            ]
        } else {
            let clear = (!finished.is_empty()).then(|| {
                let downloads = self.app.downloads().clone();
                let runtime = self.app.runtime().clone();
                Button::new("clear-finished", "Clear finished")
                    .on_click(move |_, _, _| {
                        let downloads = downloads.clone();
                        runtime.spawn(async move { downloads.clear_finished().await });
                    })
                    .into_any_element()
            });
            [
                self.section("Downloading", None, active, cx),
                self.section("Finished", clear, finished, cx),
            ]
            .into_iter()
            .flatten()
            .collect()
        };
        div()
            .id("downloads-page")
            .relative()
            .size_full()
            .text_color(theme.colors.text)
            .child(
                div()
                    .id("downloads-scroll")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    // The page starts under the floating strip and ends above the player pill.
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(theme.space.l28)
                            .px(theme.space.xl)
                            .pt(theme.size.strip)
                            .pb(theme.size.player_clearance)
                            .child(header)
                            .children(body),
                    ),
            )
            .child(Scrollbar::new("downloads-scrollbar", self.scroll.clone()))
    }
}

#[cfg(feature = "preview")]
pub mod preview {
    //! Built job lists for the Downloads page (a running and a failed job), for screenshots
    //! and tests.

    use std::time::{Duration, Instant};

    use gpui::Entity;
    use rustet_app::downloads::DownloadList;

    use super::DownloadsPage;

    /// Draws `page` from `list` from now on; `speed` (bytes per second) is shown for every
    /// downloading job.
    pub fn show_downloads(
        page: &Entity<DownloadsPage>,
        list: DownloadList,
        speed: Option<f64>,
        cx: &mut gpui::App,
    ) {
        page.update(cx, |p, cx| {
            p.frozen = true;
            let now = Instant::now();
            p.samples.clear();
            p.set_list(list.clone(), now);
            if let Some(bps) = speed {
                for s in p.samples.values_mut() {
                    s.speed = Some(bps);
                    s.at = now - Duration::from_secs(1);
                }
            }
            cx.notify();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(phase: ExportPhase) -> ExportJob {
        ExportJob {
            job_id: "j".into(),
            asset_id: "1440833098".into(),
            phase,
            title: "Garden Song".into(),
            artist_name: "Phoebe Bridgers".into(),
            ..ExportJob::default()
        }
    }

    #[test]
    fn each_job_state_draws_its_line_and_actions() {
        use JobAction as A;
        let queued = JobRow::new(
            &ExportJob {
                queue_index: 2,
                ..job(ExportPhase::Queued)
            },
            None,
        );
        assert_eq!(queued.status, JobStatus::Queued { place: 2 });
        assert_eq!(queued.line, "Queued, 2nd in line");
        assert_eq!(queued.actions, [A::DownloadNext, A::Cancel]);
        assert_eq!(queued.progress, None);

        let running = JobRow::new(
            &ExportJob {
                percent: 42,
                bytes_total: 44 << 20,
                ..job(ExportPhase::Downloading)
            },
            Some(3.0 * 1024. * 1024.),
        );
        assert_eq!(running.status, JobStatus::Running { percent: 42 });
        assert_eq!(running.line, "Downloading, 42 % of 44 MB, 3.0 MB/s");
        assert_eq!(running.progress, Some(0.42));
        assert_eq!(running.actions, [A::Cancel]);

        let cached = JobRow::new(
            &ExportJob {
                source: Some("cache".into()),
                ..job(ExportPhase::Downloading)
            },
            None,
        );
        assert_eq!(cached.line, "Downloading, 0 %, from the song cache");

        let failed = JobRow::new(
            &ExportJob {
                error: "Apple refused the stream.".into(),
                ..job(ExportPhase::Failed)
            },
            None,
        );
        assert_eq!(
            (failed.status.clone(), failed.line.as_str()),
            (JobStatus::Failed, "Apple refused the stream.")
        );
        assert_eq!(failed.actions, [A::Retry, A::Remove]);
        assert_eq!(
            JobRow::new(&job(ExportPhase::Failed), None).line,
            "The download failed. Try again."
        );

        let done = JobRow::new(
            &ExportJob {
                output: "/srv/music/a.m4a".into(),
                ..job(ExportPhase::Done)
            },
            None,
        );
        assert_eq!(
            (done.line.as_str(), done.actions.as_slice()),
            ("Saved to /srv/music/a.m4a", &[A::ShowFile, A::Remove][..])
        );

        let cancelled = JobRow::new(&job(ExportPhase::Cancelled), None);
        assert_eq!(cancelled.actions, [A::Retry, A::Remove]);

        // A job the catalog has not resolved yet shows its ID.
        let unnamed = JobRow::new(
            &ExportJob {
                title: String::new(),
                ..job(ExportPhase::Resolving)
            },
            None,
        );
        assert_eq!(
            (unnamed.title.as_str(), unnamed.line.as_str()),
            ("1440833098", "Preparing")
        );
    }
}
