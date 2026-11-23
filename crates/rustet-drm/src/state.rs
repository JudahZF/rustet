//! The DRM state snapshot and its wire format.
//!
//! The snapshot is serialised to JSON for the renderer (`GET /api/v1/drm/status`, the `drm` SSE
//! event, the sign-in sheet). Field names and state names are a contract with those scripts.

use serde::{Serialize, Serializer};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::auth::AuthChallenge;
use crate::signin_failure::SignInFailure;

/// Defines a state dimension: `Unknown` is the default and, in a [`StateUpdate`], means "no
/// change". The names are the wire strings.
macro_rules! state_enum {
    ($(#[$meta:meta])* $name:ident { $($(#[$vmeta:meta])* $variant:ident => $wire:literal,)+ }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
        pub enum $name {
            #[default]
            Unknown,
            $($(#[$vmeta])* $variant,)+
        }

        impl $name {
            /// The wire name, for example `"ready"`.
            pub const fn as_str(self) -> &'static str {
                match self {
                    Self::Unknown => "unknown",
                    $(Self::$variant => $wire,)+
                }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(self.as_str())
            }
        }
    };
}

state_enum! {
    /// The manager's own lifecycle, separate from the backend's: the manager is `Ready` while
    /// the backend is stopped (it starts lazily).
    ManagerState {
        /// DRM is not configured.
        Disabled => "disabled",
        Initializing => "initializing",
        Ready => "ready",
        ShuttingDown => "shutting_down",
        Failed => "failed",
    }
}

state_enum! {
    /// Health of the backend runtime.
    ProcessState {
        Stopped => "stopped",
        Starting => "starting",
        Running => "running",
        Failed => "failed",
    }
}

state_enum! {
    /// Where the sign-in flow is. `Challenging` means the backend asked for input (a
    /// verification code, a choice) and waits for [`crate::DrmManager::submit_challenge`];
    /// [`DrmSnapshot::challenge`] says what it asked.
    AuthState {
        LoggedOut => "logged_out",
        LoggingIn => "logging_in",
        Challenging => "challenging",
        LoggedIn => "logged_in",
        Failed => "failed",
    }
}

state_enum! {
    /// FairPlay initialisation.
    FairPlayState {
        Initializing => "initializing",
        Ready => "ready",
        Failed => "failed",
    }
}

state_enum! {
    /// What the engine knows about the persisted session.
    SessionState {
        Empty => "empty",
        Valid => "valid",
        Expired => "expired",
    }
}

state_enum! {
    /// FairPlay lease recovery. While `Refreshing`, the backend gates new decryptions.
    RecoveryState {
        Idle => "idle",
        Scheduled => "scheduled",
        Refreshing => "refreshing",
        Failed => "failed",
    }
}

/// Every dimension of the DRM subsystem. The dimensions are independent: FairPlay may be
/// initialising while authentication is logged in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct DrmState {
    pub manager: ManagerState,
    pub process: ProcessState,
    pub authentication: AuthState,
    #[serde(rename = "fairplay")]
    pub fair_play: FairPlayState,
    pub session: SessionState,
    pub recovery: RecoveryState,
}

/// Feature availability, derived from the state (FairPlay ready), never set directly.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct CapabilityState {
    pub cbcs: bool,
    pub alac: bool,
    pub atmos: bool,
    #[serde(rename = "hiRes")]
    pub hi_res: bool,
}

/// A complete point-in-time picture of the DRM subsystem. Every change publishes a full
/// snapshot, so a client that reconnects has the whole picture without replaying history.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct DrmSnapshot {
    pub state: DrmState,
    pub capabilities: CapabilityState,
    /// Set only while `state.authentication` is `Challenging`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub challenge: Option<AuthChallenge>,
    /// Why the last sign-in failed; set while `state.authentication` is `Failed`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<SignInFailure>,
    /// When the snapshot last changed. `None` (nothing emitted yet) is written as Go's zero time,
    /// which clients read as "older than anything".
    #[serde(serialize_with = "serialize_timestamp")]
    pub timestamp: Option<OffsetDateTime>,
    /// A human-readable log line. Never parsed for state.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub message: String,
}

fn serialize_timestamp<S: Serializer>(t: &Option<OffsetDateTime>, s: S) -> Result<S::Ok, S::Error> {
    match t {
        Some(t) => s.serialize_str(&t.format(&Rfc3339).map_err(serde::ser::Error::custom)?),
        None => s.serialize_str("0001-01-01T00:00:00Z"),
    }
}

/// A partial change to the snapshot: `Unknown` dimensions, a `None` challenge and a `None`
/// failure leave the current values alone (with the clearing rules of
/// [`crate::DrmManager`]'s merge). The message replaces the current one.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StateUpdate {
    pub state: DrmState,
    pub challenge: Option<AuthChallenge>,
    pub failure: Option<SignInFailure>,
    pub message: String,
}

impl StateUpdate {
    pub(crate) fn new(state: DrmState, message: impl Into<String>) -> Self {
        Self {
            state,
            message: message.into(),
            ..Self::default()
        }
    }
}

/// A state change reported by the backend.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DrmEvent {
    pub update: StateUpdate,
    /// The backend stopped because it was told to. A `Stopped` process without this flag is a
    /// crash, and the manager restarts the backend.
    pub intentional: bool,
}

impl DrmEvent {
    /// The event for a state name reported by the native library's state callback. The
    /// library reports `RUNNING` once AAC works; FairPlay counts as ready only when the
    /// Android FairPlay path is up too (`fair_play_ready`).
    pub fn from_native_state(name: &str, fair_play_ready: bool) -> Self {
        let r = parse_state_file(name);
        let fair_play = match r.fair_play {
            FairPlayState::Ready if !fair_play_ready => FairPlayState::Unknown,
            fp => fp,
        };
        let state = DrmState {
            process: r.process,
            fair_play,
            authentication: r.auth,
            recovery: r.recovery,
            ..DrmState::default()
        };
        Self {
            update: StateUpdate::new(state, format!("state change: {name}")),
            intentional: false,
        }
    }
}

/// The sub-states a native state name stands for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StateFileResult {
    pub process: ProcessState,
    pub fair_play: FairPlayState,
    pub auth: AuthState,
    pub recovery: RecoveryState,
}

/// Interprets a state name written by the native library (`write_drm_state()` in `main.c`, and
/// its state callback). Unknown names give all-`Unknown` sub-states.
pub fn parse_state_file(content: &str) -> StateFileResult {
    use {AuthState as A, FairPlayState as F, ProcessState as P, RecoveryState as R};
    let (process, fair_play, auth, recovery) = match content.trim() {
        "STARTING" => (P::Starting, F::Unknown, A::Unknown, R::Unknown),
        "LOGIN" => (P::Starting, F::Unknown, A::LoggingIn, R::Unknown),
        "WAITING_2FA" | "WAITING_DIALOG" => (P::Starting, F::Unknown, A::Challenging, R::Unknown),
        "INITIALIZING_FAIRPLAY" => (P::Running, F::Initializing, A::LoggedIn, R::Unknown),
        "RUNNING" => (P::Running, F::Ready, A::LoggedIn, R::Idle),
        "RECOVERY" => (P::Running, F::Ready, A::Unknown, R::Refreshing),
        "FAILED" => (P::Failed, F::Unknown, A::Unknown, R::Unknown),
        "STOPPED" => (P::Stopped, F::Unknown, A::Unknown, R::Unknown),
        _ => return StateFileResult::default(),
    };
    StateFileResult {
        process,
        fair_play,
        auth,
        recovery,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::auth::AuthChallengeType;
    use crate::signin_failure::FailureKind;

    #[test]
    fn parse_state_file_maps_every_native_state() {
        use {AuthState as A, FairPlayState as F, ProcessState as P, RecoveryState as R};
        let cases = [
            ("STARTING", P::Starting, F::Unknown, A::Unknown, R::Unknown),
            ("LOGIN", P::Starting, F::Unknown, A::LoggingIn, R::Unknown),
            (
                "WAITING_2FA",
                P::Starting,
                F::Unknown,
                A::Challenging,
                R::Unknown,
            ),
            (
                "WAITING_DIALOG",
                P::Starting,
                F::Unknown,
                A::Challenging,
                R::Unknown,
            ),
            (
                "INITIALIZING_FAIRPLAY",
                P::Running,
                F::Initializing,
                A::LoggedIn,
                R::Unknown,
            ),
            ("RUNNING", P::Running, F::Ready, A::LoggedIn, R::Idle),
            ("RECOVERY", P::Running, F::Ready, A::Unknown, R::Refreshing),
            ("FAILED", P::Failed, F::Unknown, A::Unknown, R::Unknown),
            ("STOPPED", P::Stopped, F::Unknown, A::Unknown, R::Unknown),
            // main.c writes the file with fprintf, so a trailing newline is normal.
            ("RUNNING\n", P::Running, F::Ready, A::LoggedIn, R::Idle),
            ("RUNNING\r\n", P::Running, F::Ready, A::LoggedIn, R::Idle),
            (
                "UNKNOWN_GARBAGE",
                P::Unknown,
                F::Unknown,
                A::Unknown,
                R::Unknown,
            ),
            ("", P::Unknown, F::Unknown, A::Unknown, R::Unknown),
        ];
        for (input, process, fair_play, auth, recovery) in cases {
            let want = StateFileResult {
                process,
                fair_play,
                auth,
                recovery,
            };
            assert_eq!(parse_state_file(input), want, "{input:?}");
        }
    }

    #[test]
    fn native_running_is_not_fair_play_ready_without_the_android_path() {
        let ev = DrmEvent::from_native_state("RUNNING", false);
        assert_eq!(ev.update.state.fair_play, FairPlayState::Unknown);
        assert_eq!(ev.update.state.process, ProcessState::Running);
        assert_eq!(
            DrmEvent::from_native_state("RUNNING", true)
                .update
                .state
                .fair_play,
            FairPlayState::Ready
        );
    }

    // The renderer (account-sheet.js, verification-code.js, drm-autoauth.mjs) reads these names.
    #[test]
    fn snapshot_wire_format() {
        let empty = serde_json::to_value(DrmSnapshot::default()).unwrap();
        assert_eq!(
            empty,
            serde_json::json!({
                "state": {"manager": "unknown", "process": "unknown", "authentication": "unknown",
                          "fairplay": "unknown", "session": "unknown", "recovery": "unknown"},
                "capabilities": {"cbcs": false, "alac": false, "atmos": false, "hiRes": false},
                "timestamp": "0001-01-01T00:00:00Z",
            })
        );

        let snap = DrmSnapshot {
            state: DrmState {
                manager: ManagerState::ShuttingDown,
                process: ProcessState::Running,
                authentication: AuthState::Challenging,
                fair_play: FairPlayState::Ready,
                session: SessionState::Valid,
                recovery: RecoveryState::Refreshing,
            },
            capabilities: CapabilityState {
                cbcs: true,
                alac: true,
                atmos: true,
                hi_res: true,
            },
            challenge: Some(AuthChallenge {
                id: 3,
                kind: AuthChallengeType::DialogChoice,
                title: "Recovery Options".into(),
                description: String::new(),
                metadata: BTreeMap::from([("buttons".into(), "Text\nCall".into())]),
            }),
            failure: Some(SignInFailure {
                kind: FailureKind::NotStoreAccount,
                message: "m".into(),
                url: None,
            }),
            timestamp: Some(time::macros::datetime!(2026-11-19 09:05:01.5 UTC)),
            message: "hello".into(),
        };
        assert_eq!(
            serde_json::to_value(snap).unwrap(),
            serde_json::json!({
                "state": {"manager": "shutting_down", "process": "running", "authentication": "challenging",
                          "fairplay": "ready", "session": "valid", "recovery": "refreshing"},
                "capabilities": {"cbcs": true, "alac": true, "atmos": true, "hiRes": true},
                "challenge": {"id": 3, "type": "dialog_choice", "title": "Recovery Options",
                              "metadata": {"buttons": "Text\nCall"}},
                "failure": {"kind": "not_store_account", "message": "m"},
                "timestamp": "2026-11-19T09:05:01.5Z",
                "message": "hello",
            })
        );
    }

    #[test]
    fn state_names() {
        let names = [
            ManagerState::Disabled.as_str(),
            ManagerState::Initializing.as_str(),
            ProcessState::Stopped.as_str(),
            ProcessState::Starting.as_str(),
            ProcessState::Failed.as_str(),
            AuthState::LoggedOut.as_str(),
            AuthState::LoggingIn.as_str(),
            AuthState::LoggedIn.as_str(),
            AuthState::Failed.as_str(),
            FairPlayState::Initializing.as_str(),
            FairPlayState::Failed.as_str(),
            SessionState::Empty.as_str(),
            SessionState::Expired.as_str(),
            RecoveryState::Idle.as_str(),
            RecoveryState::Scheduled.as_str(),
            RecoveryState::Failed.as_str(),
        ];
        assert_eq!(
            names,
            [
                "disabled",
                "initializing",
                "stopped",
                "starting",
                "failed",
                "logged_out",
                "logging_in",
                "logged_in",
                "failed",
                "initializing",
                "failed",
                "empty",
                "expired",
                "idle",
                "scheduled",
                "failed"
            ]
        );
    }
}
