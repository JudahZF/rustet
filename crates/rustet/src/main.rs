//! The `rustet` executable. One binary plays both roles:
//!
//! - `rustet [<link>] [--hidden] [--no-desktop]`: the app. It starts the engine as its child.
//! - `rustet engine --api <port> [--tls-identity-stdin]`: the engine, the local HTTPS API that
//!   signs in, decrypts and plays.
// One `unsafe`: setting an environment variable before any thread exists (`prepare_ui_process`).
#![deny(unsafe_code)]

use std::process::ExitCode;

/// What this process is.
#[derive(Debug)]
enum Role {
    Engine(rustet_engine::Options),
    App(rustet_ui_shell::Args),
}

const ENGINE_USAGE: &str = "usage: rustet engine --api <port> [--tls-identity-stdin]";

impl Role {
    /// The role from the arguments after the program name.
    fn parse(args: Vec<String>) -> Result<Self, String> {
        if args.first().map(String::as_str) != Some("engine") {
            return Ok(Self::App(rustet_ui_shell::Args::parse(args)));
        }
        let mut port = None;
        let mut tls = rustet_engine::TlsSource::Generate;
        let mut rest = args.into_iter().skip(1);
        while let Some(arg) = rest.next() {
            match arg.as_str() {
                "--api" => {
                    let value = rest.next().unwrap_or_default();
                    port = Some(value.parse::<u16>().map_err(|_| ENGINE_USAGE.to_owned())?);
                }
                "--tls-identity-stdin" => tls = rustet_engine::TlsSource::Stdin,
                _ => return Err(ENGINE_USAGE.to_owned()),
            }
        }
        let port = port.ok_or_else(|| ENGINE_USAGE.to_owned())?;
        Ok(Self::Engine(rustet_engine::Options { port, tls }))
    }
}

fn main() -> ExitCode {
    match Role::parse(std::env::args().skip(1).collect()) {
        Ok(Role::Engine(options)) => run_engine(options),
        Ok(Role::App(args)) => {
            prepare_ui_process();
            run_app(args)
        }
        Err(usage) => {
            eprintln!("{usage}");
            ExitCode::from(2)
        }
    }
}

/// The variable that lets wgpu use a Vulkan driver that does not claim conformance.
const NONCOMPLIANT_VULKAN: &str = "WGPU_ALLOW_UNDERLYING_NONCOMPLIANT_ADAPTER";

/// On Apple Silicon Linux (Asahi), lets gpui render through Mesa's Vulkan driver, which wgpu
/// hides because this Mesa reports no conformance. On its OpenGL fallback each music-video frame
/// upload costs 25–30 ms of the main thread. A value the user set is kept (`0` keeps OpenGL);
/// if Vulkan fails to start, gpui still falls back to OpenGL. The patched `gpui_wgpu` reads the
/// variable (third_party/gpui_wgpu/README.md).
#[allow(unsafe_code)]
fn prepare_ui_process() {
    let compatible = std::fs::read("/proc/device-tree/compatible").unwrap_or_default();
    if std::env::var_os(NONCOMPLIANT_VULKAN).is_none() && is_apple_silicon(&compatible) {
        // SAFETY: the first thing `main` does for the app, before any thread exists, so nothing
        // reads the environment while it changes.
        unsafe { std::env::set_var(NONCOMPLIANT_VULKAN, "1") };
    }
}

/// Whether a device-tree `compatible` list (NUL-separated) names an Apple Silicon machine.
fn is_apple_silicon(compatible: &[u8]) -> bool {
    compatible
        .split(|b| *b == 0)
        .any(|entry| entry.starts_with(b"apple,"))
}

fn run_app(args: rustet_ui_shell::Args) -> ExitCode {
    rustet_ui_shell::run(
        args,
        rustet_ui_shell::Views {
            page: rustet_ui_pages::page_area,
            settings: rustet_ui_panels::settings_page,
            downloads: rustet_ui_panels::downloads_page,
        },
    )
}

fn run_engine(options: rustet_engine::Options) -> ExitCode {
    // Before the runtime starts its threads.
    rustet_engine::prepare_process();
    let result = tokio::runtime::Runtime::new()
        .map_err(anyhow::Error::from)
        .and_then(|rt| rt.block_on(rustet_engine::run(options)));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("rustet engine: {e:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apple_silicon_is_read_from_the_device_tree() {
        // A MacBook Pro (M2 Pro) under Asahi Linux, and a Raspberry Pi 4.
        assert!(is_apple_silicon(
            b"apple,j414s\0apple,t6020\0apple,arm-platform\0"
        ));
        assert!(!is_apple_silicon(b"raspberrypi,4-model-b\0brcm,bcm2711\0"));
        assert!(!is_apple_silicon(b""), "no device tree (x86)");
    }

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn engine_role_and_its_flags() {
        use rustet_engine::TlsSource;
        let engine = |a: &[&str]| match Role::parse(args(a)) {
            Ok(Role::Engine(o)) => Some((o.port, o.tls)),
            _ => None,
        };
        assert_eq!(
            engine(&["engine", "--api", "20025", "--tls-identity-stdin"]),
            Some((20025, TlsSource::Stdin))
        );
        assert_eq!(
            engine(&["engine", "--api", "20125"]),
            Some((20125, TlsSource::Generate))
        );
        for bad in [
            &["engine"][..],
            &["engine", "--api"],
            &["engine", "--api", "70000"],
            &["engine", "--api", "1", "--other"],
        ] {
            assert!(Role::parse(args(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn anything_else_is_the_app() {
        let Ok(Role::App(app)) = Role::parse(args(&["rustet://album/1440833098", "--hidden"]))
        else {
            panic!("not the app");
        };
        assert_eq!(app.link.as_deref(), Some("rustet://album/1440833098"));
        assert!(app.hidden);
    }
}
