//! The system tray icon (StatusNotifierItem on the session bus) and its menu.
//!
//! The menu is the Electron shell's: Show/Hide Window, Maximise/Restore, Play/Pause, Next,
//! Previous, Mini Player (or Hide Mini Player while it is open), Restart App, Exit. A left click
//! (`Activate`) toggles the window. Many tray hosts only open the menu, so Show/Hide is the
//! reliable path. Every action is a [`TrayEvent`] for the shell, which owns the windows.
//!
//! The item stays registered when no tray host is running yet and appears when one starts.

use tokio::sync::mpsc;

use crate::command::Transport;

/// A tray action for the shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEvent {
    /// Hide the main window if it is visible, else show and focus it.
    ToggleWindow,
    /// Maximise the main window, or restore it if maximised.
    ToggleMaximize,
    /// Send to the page on `mpris:cmd`.
    Playback(Transport),
    /// Open or close the mini player.
    ToggleMiniPlayer,
    /// Restart: clear the stored `rustet_resume_state` first, so the restart starts fresh.
    Restart,
    Quit,
}

#[derive(Debug, thiserror::Error)]
pub enum TrayError {
    #[error("tray icon is not a readable PNG: {0}")]
    Icon(#[from] png::DecodingError),
    #[error("tray icon has an unsupported PNG colour type")]
    IconFormat,
    #[error(transparent)]
    Register(#[from] ksni::Error),
}

/// Largest icon edge sent to the tray host. Hosts draw 16–48 px; the 512 px asset would
/// otherwise be a 1 MiB property on every read.
const ICON_MAX: u32 = 64;

/// Decodes a PNG into the ARGB32 (network byte order) pixmap of the StatusNotifierItem spec,
/// scaled down by an integer factor to at most [`ICON_MAX`] pixels per edge.
pub fn icon_from_png(png_bytes: &[u8]) -> Result<ksni::Icon, TrayError> {
    let mut decoder = png::Decoder::new(png_bytes);
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info()?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf)?;
    let px = &buf[..info.buffer_size()];
    let rgba: Vec<[u8; 4]> = match info.color_type {
        png::ColorType::Rgba => px.as_chunks::<4>().0.to_vec(),
        png::ColorType::Rgb => px
            .as_chunks::<3>()
            .0
            .iter()
            .map(|&[r, g, b]| [r, g, b, 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => px
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&[g, a]| [g, g, g, a])
            .collect(),
        png::ColorType::Grayscale => px.iter().map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => return Err(TrayError::IconFormat),
    };
    let (w, h) = (info.width, info.height);
    let f = w.max(h).div_ceil(ICON_MAX).max(1);
    let (ow, oh) = ((w / f).max(1), (h / f).max(1));
    let mut data = Vec::with_capacity((ow * oh * 4) as usize);
    for oy in 0..oh {
        for ox in 0..ow {
            // Alpha-weighted box filter, so transparent pixels do not darken the edges.
            let mut sum = [0u64; 4];
            for y in oy * f..((oy + 1) * f).min(h) {
                for x in ox * f..((ox + 1) * f).min(w) {
                    let [r, g, b, a] = rgba[(y * w + x) as usize];
                    let a64 = u64::from(a);
                    sum[0] += u64::from(r) * a64;
                    sum[1] += u64::from(g) * a64;
                    sum[2] += u64::from(b) * a64;
                    sum[3] += a64;
                }
            }
            let n = u64::from(f * f);
            let colour = |c: u64| c.checked_div(sum[3]).unwrap_or(0) as u8;
            data.extend_from_slice(&[
                (sum[3] / n) as u8,
                colour(sum[0]),
                colour(sum[1]),
                colour(sum[2]),
            ]);
        }
    }
    Ok(ksni::Icon {
        width: ow as i32,
        height: oh as i32,
        data,
    })
}

struct RustetTray {
    icon: ksni::Icon,
    mini_open: bool,
    events: mpsc::UnboundedSender<TrayEvent>,
}

impl RustetTray {
    fn emit(&self, e: TrayEvent) {
        let _ = self.events.send(e);
    }
}

fn item(label: &str, event: TrayEvent) -> ksni::MenuItem<RustetTray> {
    ksni::menu::StandardItem {
        label: label.into(),
        activate: Box::new(move |t: &mut RustetTray| t.emit(event)),
        ..Default::default()
    }
    .into()
}

impl ksni::Tray for RustetTray {
    fn id(&self) -> String {
        "rustet".into()
    }

    fn title(&self) -> String {
        "Rustet".into()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: "Rustet".into(),
            ..Default::default()
        }
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        vec![self.icon.clone()]
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.emit(TrayEvent::ToggleWindow);
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::MenuItem::Separator;
        vec![
            item("Show/Hide Window", TrayEvent::ToggleWindow),
            item("Maximise/Restore", TrayEvent::ToggleMaximize),
            Separator,
            item("Play/Pause", TrayEvent::Playback(Transport::PlayPause)),
            item("Next", TrayEvent::Playback(Transport::Next)),
            item("Previous", TrayEvent::Playback(Transport::Previous)),
            Separator,
            item(
                if self.mini_open {
                    "Hide Mini Player"
                } else {
                    "Mini Player"
                },
                TrayEvent::ToggleMiniPlayer,
            ),
            Separator,
            item("Restart App", TrayEvent::Restart),
            item("Exit", TrayEvent::Quit),
        ]
    }
}

/// Handle to the tray icon. [`Tray::shutdown`] removes it.
pub struct Tray {
    handle: ksni::Handle<RustetTray>,
}

impl Tray {
    /// Registers the tray item on the session bus. `icon_png` is the tray icon
    /// (`assets/tray-icon.png`). Menu actions arrive on the returned receiver.
    pub async fn spawn(
        icon_png: &[u8],
    ) -> Result<(Self, mpsc::UnboundedReceiver<TrayEvent>), TrayError> {
        use ksni::TrayMethods;
        let (events, rx) = mpsc::unbounded_channel();
        let tray = RustetTray {
            icon: icon_from_png(icon_png)?,
            mini_open: false,
            events,
        };
        let handle = tray.assume_sni_available(true).spawn().await?;
        Ok((Self { handle }, rx))
    }

    /// Whether the mini player is open, for the label of its menu item.
    pub async fn set_mini_player_open(&self, open: bool) {
        self.handle.update(|t| t.mini_open = open).await;
    }

    pub async fn shutdown(self) {
        self.handle.shutdown().await;
    }
}
