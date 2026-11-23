//! Loads the FairPlay libraries and runs the signed-out start, to check an installation
//! without an account. Never signs in: it gives no credentials and cancels every prompt.
//!
//! ```text
//! cargo run -p rustet-drm-native --example probe -- <libhybris-core.so> <linker dir> <lib64 dir> <empty base dir> [start]
//! ```
//!
//! Without `start` it stops after loading the libraries (what the engine does at start-up
//! while signed out); with `start` it also runs the FairPlay start, which ends in a cancelled
//! silent sign-in. Use a fresh base directory: the start writes `mpl_db/` there. Set
//! `RUST_LOG=debug` (or `trace` for every symbol lookup) for the wrapper's checkpoints.

#[cfg(target_os = "linux")]
fn main() {
    use std::path::PathBuf;
    use std::sync::Arc;

    use rustet_drm_native::{FairPlayConfig, Hybris, HybrisPaths, SignInPrompt, VerificationReply};

    struct Cancel;

    impl SignInPrompt for Cancel {
        fn verification_code(&self) -> VerificationReply {
            eprintln!("[probe] verification code requested: cancelling");
            VerificationReply::Cancel
        }
        fn dialog_choice(&self, title: &str, buttons: &[String]) -> usize {
            eprintln!("[probe] dialog {title:?} {buttons:?}: first button");
            0
        }
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let [core, linker, lib64, base, rest @ ..] = args.as_slice() else {
        eprintln!(
            "usage: probe <libhybris-core.so> <linker dir> <lib64 dir> <empty base dir> [start]"
        );
        std::process::exit(2);
    };
    let paths = HybrisPaths {
        core: core.into(),
        linker_dir: linker.into(),
        lib64_dir: lib64.into(),
    };
    let base = PathBuf::from(base);
    std::fs::create_dir_all(base.join("mpl_db")).expect("create base dir");

    println!(
        "stored account: {:?}",
        rustet_drm_native::stored_account(&base)
    );
    let hybris = match Hybris::load(&paths) {
        Ok(h) => h,
        Err(e) => {
            println!("Hybris::load failed: {e}");
            std::process::exit(1);
        }
    };
    println!("Hybris::load ok");

    if rest.first().map(String::as_str) == Some("start") {
        let cfg = FairPlayConfig {
            base_dir: base,
            lib64_dir: paths.lib64_dir.clone(),
            device_info: String::new(),
            credentials: None,
            prompt: Arc::new(Cancel),
        };
        match hybris.start_fair_play(cfg) {
            Ok(_) => println!("start_fair_play ok"),
            Err(e) => println!("start_fair_play failed: {e} ({e:?})"),
        }
    }
    // Apple's static destructors can hang at exit (the C build did too): leave at once.
    // SAFETY: _exit ends the process without running any more code of ours.
    unsafe { libc::_exit(0) };
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("the FairPlay session runs on Linux only");
}
