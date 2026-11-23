//! The engine child process: where its data lives, its config, how it is started and
//! supervised, and how the shell talks to it.
//!
//! The engine is this same executable started as `rustet engine --api <port>`, working directory
//! `<config>/engine-data`.

pub mod client;
pub mod orphan;
pub mod supervisor;

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::platform::{EngineLibs, Env, Platform, engine_lib_env, yaml_str};
pub use client::{EngineApi, EngineClient, EngineResponse};
pub use supervisor::{EngineEvent, EngineLauncher, EngineSupervisor, SupervisorTiming};

/// The engine's loopback port.
pub const ENGINE_PORT: u16 = 20025;

/// Why the engine could not start.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("engine binary not found at {}", .0.display())]
    BinaryMissing(PathBuf),
    #[error(
        "engine port {0} is already in use. Close the other player or service and restart Rustet."
    )]
    PortInUse(u16),
    #[error("engine config: {0}")]
    Config(#[source] io::Error),
    #[error("engine could not start: {0}")]
    Spawn(#[source] io::Error),
}

/// The engine's working directory (config, caches).
pub fn engine_data_dir(config_dir: &Path) -> PathBuf {
    config_dir.join("engine-data")
}

/// The folders the user picked for exports, which the engine may write to (`RUSTET_EXPORT_PICKED`).
pub fn export_picked_file(config_dir: &Path) -> PathBuf {
    config_dir.join("export-folders.txt")
}

/// Where Apple's FairPlay libraries are installed.
pub fn fairplay_lib64(config_dir: &Path) -> PathBuf {
    config_dir.join("drm/rootfs/system/lib64")
}

/// Where the DRM session lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DrmLayout {
    /// An installed build: the session lives in the user's config directory, because the install
    /// directory can be a read-only AppImage mount, `/opt` or a Flatpak `/app`. `legacy` is the
    /// install directory's own `drm/files`, where older builds kept it; it is copied once.
    Installed { legacy: Option<PathBuf> },
    /// A run from the source tree: the repository's own `drm/files`.
    Source { repo_root: PathBuf },
}

/// The engine's Apple session directory (session files, device trust, session lock).
pub fn drm_files_dir(config_dir: &Path, layout: &DrmLayout) -> PathBuf {
    match layout {
        DrmLayout::Installed { .. } => config_dir.join("drm/files"),
        DrmLayout::Source { repo_root } => repo_root.join("drm/files"),
    }
}

/// Creates `<config>/drm`, removes artefacts of the old hybris-based DRM path, and returns the
/// session directory `<config>/drm/files` (`ensureUserDRM`).
pub fn ensure_user_drm(config_dir: &Path) -> io::Result<PathBuf> {
    let user = config_dir.join("drm");
    fs::create_dir_all(&user)?;
    for stale in ["drm-rootless", "drm-native", "hybris-linker"] {
        let _ = fs::remove_file(user.join(stale));
    }
    Ok(user.join("files"))
}

fn copy_dir(from: &Path, to: &Path) -> io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

/// The session directory for `layout`, created (and migrated from the install directory once).
pub fn prepare_drm_base(config_dir: &Path, layout: &DrmLayout) -> io::Result<PathBuf> {
    match layout {
        DrmLayout::Source { .. } => Ok(drm_files_dir(config_dir, layout)),
        DrmLayout::Installed { legacy } => {
            let base = ensure_user_drm(config_dir)?;
            if let Some(legacy) = legacy.as_deref().filter(|l| !base.exists() && l.exists())
                && let Err(e) = copy_dir(legacy, &base)
            {
                tracing::error!("could not migrate the DRM session: {e}");
            }
            fs::create_dir_all(&base)?;
            Ok(base)
        }
    }
}

/// Writes `<data>/config.yaml` with the defaults, or patches an existing one: removes the
/// obsolete `drm-binary-path` line and adds or updates `drm-base-dir` (`ensureEngineConfig`).
pub fn ensure_engine_config(engine_data: &Path, drm_base: &Path) -> io::Result<()> {
    fs::create_dir_all(engine_data)?;
    let path = engine_data.join("config.yaml");
    let base_line = format!("drm-base-dir: {}", yaml_str(&drm_base.to_string_lossy()));
    let Ok(original) = fs::read_to_string(&path) else {
        let stub = [
            "storefront: us",
            "media-user-token: \"\"",
            "authorization-token: \"\"",
            "decrypt-m3u8-port: 127.0.0.1:10020",
            "get-m3u8-port: 127.0.0.1:20020",
            "get-m3u8-from-device: true",
            "get-m3u8-mode: hires",
            "aac-type: aac-lc",
            "alac-max: 192000",
            &base_line,
        ];
        return fs::write(&path, stub.join("\n") + "\n");
    };
    let mut content = original.clone();
    // The first line that starts with `drm-binary-path:`, with its newline.
    let line_start = |text: &str, key: &str| {
        let mut at = 0;
        text.split_inclusive('\n').find_map(|line| {
            let start = at;
            at += line.len();
            line.starts_with(key).then_some(start..at)
        })
    };
    if let Some(range) = line_start(&content, "drm-binary-path:") {
        content.replace_range(range, "");
    }
    if let Some(i) = content.find("drm-base-dir:") {
        if !content.contains(&base_line) {
            let end = content[i..].find('\n').map_or(content.len(), |n| i + n);
            content.replace_range(i..end, &base_line);
        }
    } else {
        content.push_str(&base_line);
        content.push('\n');
    }
    if content != original {
        fs::write(&path, content)?;
    }
    Ok(())
}

/// What to run as the engine. The real one is this executable with `engine`; tests run fakes.
#[derive(Debug, Clone)]
pub struct EngineProgram {
    pub program: PathBuf,
    /// Arguments before `--api <port>`.
    pub args: Vec<OsString>,
}

impl EngineProgram {
    /// `rustet engine`, from `std::env::current_exe()`.
    pub fn current_exe() -> io::Result<Self> {
        Ok(Self {
            program: std::env::current_exe()?,
            args: vec!["engine".into()],
        })
    }
}

/// The real engine launcher.
#[derive(Debug, Clone)]
pub struct RustetEngine {
    pub program: EngineProgram,
    pub port: u16,
    pub config_dir: PathBuf,
    pub drm: DrmLayout,
    /// The bundled VLC and FFmpeg the engine loads.
    pub libs: EngineLibs,
    /// The environment the shell started with.
    pub env: Env,
    /// `/proc` outside tests (orphan recovery).
    pub proc_root: PathBuf,
    /// The TLS identity as PEM (`rustet_engine::TlsIdentity::to_pem`), handed to every start of
    /// the engine on its standard input (`--tls-identity-stdin`). The shell made it and pinned
    /// its key in Chromium, so every restart must serve the same one. `None`: the engine
    /// generates its own.
    pub tls_identity: Option<String>,
}

impl EngineLauncher for RustetEngine {
    async fn prepare(&self, quit: &CancellationToken) -> Result<(), EngineError> {
        if !self.program.program.exists() {
            return Err(EngineError::BinaryMissing(self.program.program.clone()));
        }
        let data = engine_data_dir(&self.config_dir);
        #[cfg(unix)]
        {
            let lock = drm_files_dir(&self.config_dir, &self.drm).join("engine-session.lock");
            orphan::recover_orphan_engine(
                &self.proc_root,
                &lock,
                &self.program.program,
                &data,
                self.port,
            )
            .await;
        }
        let mut free = false;
        for _ in 0..20 {
            if quit.is_cancelled() {
                return Ok(());
            }
            if orphan::is_port_free(self.port) {
                free = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if quit.is_cancelled() {
            return Ok(());
        }
        if !free {
            return Err(EngineError::PortInUse(self.port));
        }
        let drm_base =
            prepare_drm_base(&self.config_dir, &self.drm).map_err(EngineError::Config)?;
        ensure_engine_config(&data, &drm_base).map_err(EngineError::Config)
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(&self.program.program);
        cmd.args(&self.program.args)
            .arg("--api")
            .arg(self.port.to_string())
            .args(
                self.tls_identity
                    .is_some()
                    .then_some("--tls-identity-stdin"),
            )
            .current_dir(engine_data_dir(&self.config_dir))
            .env("RUSTET_EXPORT_PICKED", export_picked_file(&self.config_dir))
            .envs(engine_lib_env(Platform::current(), &self.env, &self.libs));
        cmd
    }

    fn stdin(&self) -> Option<Vec<u8>> {
        self.tls_identity.clone().map(String::into_bytes)
    }
}

/// The bundled native libraries under `resources`: `vlc/` and `ffmpeg/`. The bundled FFmpeg is
/// x86-64 only; on other CPUs the engine uses the host's ffmpeg from `PATH`.
pub fn bundled_libs(resources: &Path) -> EngineLibs {
    let vlc = resources.join("vlc");
    let ff = resources.join("ffmpeg");
    let ff_exe = if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    };
    EngineLibs {
        has_vlc: vlc.exists(),
        has_ffmpeg: cfg!(target_arch = "x86_64") && ff.join("bin").join(ff_exe).exists(),
        vlc_dir: vlc.to_string_lossy().into_owned(),
        ffmpeg_dir: ff.to_string_lossy().into_owned(),
        resources_dir: Some(resources.to_string_lossy().into_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installed_builds_keep_the_session_in_the_config_dir_and_migrate_once() {
        let cfg = tempfile::tempdir().unwrap();
        let install = tempfile::tempdir().unwrap();
        let legacy = install.path().join("drm/files");
        fs::create_dir_all(legacy.join("mpl_db")).unwrap();
        fs::write(legacy.join("MUSIC_TOKEN"), "tok").unwrap();
        fs::write(legacy.join("mpl_db/x"), "db").unwrap();
        fs::create_dir_all(cfg.path().join("drm")).unwrap();
        fs::write(cfg.path().join("drm/hybris-linker"), "").unwrap();
        let layout = DrmLayout::Installed {
            legacy: Some(legacy.clone()),
        };
        let base = prepare_drm_base(cfg.path(), &layout).unwrap();
        assert_eq!(base, cfg.path().join("drm/files"));
        assert_eq!(base, drm_files_dir(cfg.path(), &layout));
        assert_eq!(fs::read_to_string(base.join("mpl_db/x")).unwrap(), "db");
        assert!(!cfg.path().join("drm/hybris-linker").exists());
        fs::write(legacy.join("MUSIC_TOKEN"), "newer").unwrap();
        prepare_drm_base(cfg.path(), &layout).unwrap();
        assert_eq!(
            fs::read_to_string(base.join("MUSIC_TOKEN")).unwrap(),
            "tok",
            "migrated only once"
        );
        let src = DrmLayout::Source {
            repo_root: install.path().into(),
        };
        assert_eq!(drm_files_dir(cfg.path(), &src), legacy);
    }

    #[test]
    fn engine_config_is_created_then_patched() {
        let d = tempfile::tempdir().unwrap();
        let data = d.path().join("engine-data");
        ensure_engine_config(&data, Path::new("/c/rustet/drm/files")).unwrap();
        let text = fs::read_to_string(data.join("config.yaml")).unwrap();
        assert!(text.starts_with("storefront: us\n"));
        assert!(text.ends_with("drm-base-dir: \"/c/rustet/drm/files\"\n"));

        fs::write(
            data.join("config.yaml"),
            "storefront: gb\ndrm-binary-path: /old/bin\ndrm-base-dir: \"/old\"\nalac-max: 96000\n",
        )
        .unwrap();
        ensure_engine_config(&data, Path::new(r"C:\Users\a\rustet\drm\files")).unwrap();
        assert_eq!(
            fs::read_to_string(data.join("config.yaml")).unwrap(),
            "storefront: gb\ndrm-base-dir: \"C:\\\\Users\\\\a\\\\rustet\\\\drm\\\\files\"\nalac-max: 96000\n"
        );
        fs::write(data.join("config.yaml"), "storefront: gb").unwrap();
        ensure_engine_config(&data, Path::new("/x")).unwrap();
        assert_eq!(
            fs::read_to_string(data.join("config.yaml")).unwrap(),
            "storefront: gbdrm-base-dir: \"/x\"\n",
            "appended as before"
        );
    }

    #[test]
    fn the_engine_command() {
        let cfg = tempfile::tempdir().unwrap();
        let engine = RustetEngine {
            program: EngineProgram {
                program: "/usr/bin/rustet".into(),
                args: vec!["engine".into()],
            },
            port: 20025,
            config_dir: cfg.path().into(),
            drm: DrmLayout::Installed { legacy: None },
            libs: EngineLibs::default(),
            env: Env::default(),
            proc_root: "/proc".into(),
            tls_identity: None,
        };
        let cmd = engine.command();
        let std = cmd.as_std();
        assert_eq!(std.get_program(), "/usr/bin/rustet");
        assert_eq!(
            std.get_args().collect::<Vec<_>>(),
            ["engine", "--api", "20025"]
        );
        assert_eq!(
            std.get_current_dir(),
            Some(cfg.path().join("engine-data").as_path())
        );
        let envs: Vec<_> = std.get_envs().collect();
        assert!(envs.iter().any(|(k, v)| *k == "RUSTET_EXPORT_PICKED"
            && v.is_some_and(|v| Path::new(v) == cfg.path().join("export-folders.txt"))));
        assert!(engine.stdin().is_none());

        // With the shell's identity: the engine reads it from standard input.
        let engine = RustetEngine {
            tls_identity: Some("PEM".into()),
            ..engine
        };
        assert_eq!(
            engine.command().as_std().get_args().collect::<Vec<_>>(),
            ["engine", "--api", "20025", "--tls-identity-stdin"]
        );
        assert_eq!(engine.stdin().as_deref(), Some(&b"PEM"[..]));
    }

    #[tokio::test]
    async fn prepare_reports_a_missing_binary_and_a_busy_port() {
        let cfg = tempfile::tempdir().unwrap();
        let mut engine = RustetEngine {
            program: EngineProgram {
                program: cfg.path().join("missing"),
                args: vec![],
            },
            port: 0,
            config_dir: cfg.path().into(),
            drm: DrmLayout::Installed { legacy: None },
            libs: EngineLibs::default(),
            env: Env::default(),
            proc_root: cfg.path().join("proc"),
            tls_identity: None,
        };
        let quit = CancellationToken::new();
        assert!(matches!(
            engine.prepare(&quit).await,
            Err(EngineError::BinaryMissing(_))
        ));
        engine.program.program = std::env::current_exe().unwrap();
        let busy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        engine.port = busy.local_addr().unwrap().port();
        assert!(matches!(
            engine.prepare(&quit).await,
            Err(EngineError::PortInUse(_))
        ));
        drop(busy);
        engine.prepare(&quit).await.unwrap();
        assert!(cfg.path().join("engine-data/config.yaml").exists());
        assert!(cfg.path().join("drm/files").is_dir());
    }
}
