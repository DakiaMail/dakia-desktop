use anyhow::Error;
use std::sync::{Arc, Mutex};
use tauri::{App, Manager, Runtime, WebviewUrl, WebviewWindowBuilder, WindowEvent};

const EXIT_CODE_STARTUP_STORAGE_FAILURE: i32 = 1;
pub(crate) const FATAL_RECOVERY_WINDOW_LABEL: &str = "fatal-recovery";

type StartupFailureExitCallback = Box<dyn FnOnce() + Send>;
type StartupFailureExitCallbackState = Arc<Mutex<Option<StartupFailureExitCallback>>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StartupStorageFailureKind {
    NewerProfile,
    UpgradeFailed,
    StorageUnavailable,
}

impl StartupStorageFailureKind {
    pub(crate) fn fatal_recovery_reason(self) -> &'static str {
        match self {
            Self::NewerProfile => "newer_profile",
            Self::UpgradeFailed => "upgrade_failed",
            Self::StorageUnavailable => "storage_unavailable",
        }
    }

    fn diagnostic_code(self) -> &'static str {
        match self {
            Self::NewerProfile => "newer_profile",
            Self::UpgradeFailed => "profile_upgrade_failed",
            Self::StorageUnavailable => "profile_unavailable",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct StartupStorageFailure {
    pub(crate) kind: StartupStorageFailureKind,
    pub(crate) title: &'static str,
    pub(crate) message: &'static str,
}

impl StartupStorageFailure {
    fn from_error(error: &Error) -> Self {
        let kind = error
            .chain()
            .map(ToString::to_string)
            .find_map(|detail| {
                if detail.starts_with("profile schema version ")
                    && detail.contains(" is newer than this Dakia build supports (")
                {
                    Some(StartupStorageFailureKind::NewerProfile)
                } else if detail.starts_with("profile schema validation failed:")
                    || detail.starts_with("stored profile_schema_version is invalid:")
                    || detail.starts_with("stored profile_schema_migration_target is invalid:")
                {
                    Some(StartupStorageFailureKind::UpgradeFailed)
                } else {
                    None
                }
            })
            .unwrap_or(StartupStorageFailureKind::StorageUnavailable);

        match kind {
            StartupStorageFailureKind::NewerProfile => Self {
                kind,
                title: "This mail data needs a newer Dakia version",
                message: "Dakia stopped before migrating this mail data because a newer version created it. Open it with that newer Dakia version. To return to an older version, quit Dakia and restore the complete backup made before the upgrade.",
            },
            StartupStorageFailureKind::UpgradeFailed => Self {
                kind,
                title: "Dakia couldn't safely upgrade your mail data",
                message: "Dakia stopped instead of opening an empty mailbox. Keep the complete Dakia data folder for recovery. Restore the complete backup made before the upgrade, or reopen it with a compatible newer Dakia version.",
            },
            StartupStorageFailureKind::StorageUnavailable => Self {
                kind,
                title: "Dakia couldn't safely open your mail data",
                message: "Dakia stopped instead of opening an empty mailbox. Quit every Dakia window and try once more. If it still fails, keep the complete Dakia data folder for recovery and restore a complete backup before changing it.",
            },
        }
    }

    pub(crate) fn fatal_recovery_path(self) -> String {
        format!(
            "/fatal-recovery.html?reason={}",
            self.kind.fatal_recovery_reason()
        )
    }
}

pub(crate) trait StartupFailurePresenter {
    fn isolate_mailbox_surfaces(&mut self) -> bool;
    fn show_error(&mut self, failure: StartupStorageFailure, on_closed: Box<dyn FnOnce() + Send>);
}

pub(crate) struct TauriStartupFailurePresenter<'a, R: Runtime> {
    app: &'a App<R>,
}

impl<'a, R: Runtime> TauriStartupFailurePresenter<'a, R> {
    pub(crate) fn new(app: &'a App<R>) -> Self {
        Self { app }
    }
}

impl<R: Runtime> StartupFailurePresenter for TauriStartupFailurePresenter<'_, R> {
    fn isolate_mailbox_surfaces(&mut self) -> bool {
        isolate_mailbox_surfaces(self.app)
    }

    fn show_error(&mut self, failure: StartupStorageFailure, on_closed: Box<dyn FnOnce() + Send>) {
        // This deliberately avoids the system dialog API. The recovery page is
        // a separate static webview that does not boot the mail application or
        // receive a general window capability.
        let callback = Arc::new(Mutex::new(Some(on_closed)));
        let callback_for_close = callback.clone();
        let window = WebviewWindowBuilder::new(
            self.app,
            FATAL_RECOVERY_WINDOW_LABEL,
            WebviewUrl::App(failure.fatal_recovery_path().into()),
        )
        .title("Dakia")
        .inner_size(560.0, 320.0)
        .min_inner_size(560.0, 320.0)
        .max_inner_size(560.0, 320.0)
        .center()
        .focused(true)
        .resizable(false)
        .decorations(true)
        .build();

        match window {
            Ok(window) => {
                window.on_window_event(move |event| {
                    if matches!(event, WindowEvent::CloseRequested { .. }) {
                        run_startup_failure_exit(&callback_for_close);
                    }
                });
            }
            Err(error) => {
                tracing::error!(
                    category = "startup_recovery_window_failed",
                    error = %error,
                    "Dakia could not present its fatal recovery window"
                );
                run_startup_failure_exit(&callback);
            }
        }
    }
}

fn run_startup_failure_exit(callback: &StartupFailureExitCallbackState) {
    if let Some(callback) = callback
        .lock()
        .expect("startup recovery exit callback lock poisoned")
        .take()
    {
        callback();
    }
}

fn isolate_mailbox_surfaces<R: Runtime>(app: &App<R>) -> bool {
    // Configured webviews exist before Tauri calls application setup. Hide their
    // native windows and remove every webview before setup returns without
    // AppState, so page code cannot invoke a command against unmanaged state.
    // Keep the hidden native windows alive until the dialog callback exits the
    // app: destroying the last window can stop Tauri before the alert is read.
    let mut isolated = true;
    let surfaces = app.webview_windows().into_values().collect::<Vec<_>>();
    for surface in &surfaces {
        if surface.hide().is_err() {
            isolated = false;
        }
    }
    for surface in &surfaces {
        if AsRef::<tauri::Webview<R>>::as_ref(surface).close().is_err() {
            isolated = false;
        }
    }
    isolated
}

pub(crate) fn present_startup_storage_failure(
    presenter: &mut impl StartupFailurePresenter,
    error: &Error,
    on_closed: impl FnOnce() + Send + 'static,
) -> StartupStorageFailureKind {
    let failure = StartupStorageFailure::from_error(error);
    tracing::error!(
        category = failure.kind.diagnostic_code(),
        "Dakia stopped before opening its mail profile"
    );
    if !presenter.isolate_mailbox_surfaces() {
        tracing::error!(
            category = "startup_surface_isolation_failed",
            "Dakia could not isolate its mailbox surface after a startup failure"
        );
        on_closed();
        return failure.kind;
    }
    presenter.show_error(failure, Box::new(on_closed));
    failure.kind
}

pub(crate) fn startup_storage_failure_exit_code() -> i32 {
    EXIT_CODE_STARTUP_STORAGE_FAILURE
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };

    struct RecordingPresenter {
        events: Vec<&'static str>,
        failure: Option<StartupStorageFailure>,
        on_closed: Option<Box<dyn FnOnce() + Send>>,
        isolation_succeeds: bool,
    }

    impl Default for RecordingPresenter {
        fn default() -> Self {
            Self {
                events: Vec::new(),
                failure: None,
                on_closed: None,
                isolation_succeeds: true,
            }
        }
    }

    impl StartupFailurePresenter for RecordingPresenter {
        fn isolate_mailbox_surfaces(&mut self) -> bool {
            self.events.push("isolated-mailbox-surfaces");
            self.isolation_succeeds
        }

        fn show_error(
            &mut self,
            failure: StartupStorageFailure,
            on_closed: Box<dyn FnOnce() + Send>,
        ) {
            self.events.push("showed-fatal-recovery-window");
            self.failure = Some(failure);
            self.on_closed = Some(on_closed);
        }
    }

    #[test]
    fn newer_profile_gets_specific_backup_recovery_copy_without_raw_details() {
        let error =
            anyhow::anyhow!("profile schema version 2 is newer than this Dakia build supports (1)")
                .context("opening /private/example/subject-secret/dakia.db password=hunter2");
        let failure = StartupStorageFailure::from_error(&error);

        assert_eq!(failure.kind, StartupStorageFailureKind::NewerProfile);
        assert!(failure.title.contains("newer Dakia version"));
        assert!(failure.message.contains("complete backup"));
        for prohibited in [
            "/private/example",
            "subject-secret",
            "hunter2",
            "schema version 2",
            "SELECT",
        ] {
            assert!(!failure.title.contains(prohibited));
            assert!(!failure.message.contains(prohibited));
        }
    }

    #[test]
    fn conflicting_migration_gets_failed_upgrade_recovery_copy() {
        let error = anyhow::anyhow!(
            "profile schema validation failed: send_attempts.payload_hash is missing"
        );
        let failure = StartupStorageFailure::from_error(&error);

        assert_eq!(failure.kind, StartupStorageFailureKind::UpgradeFailed);
        assert!(failure.title.contains("safely upgrade"));
        assert!(failure
            .message
            .contains("instead of opening an empty mailbox"));
        assert!(failure.message.contains("complete backup"));
        assert!(!failure.message.contains("send_attempts"));
        assert!(!failure.message.contains("payload_hash"));
    }

    #[test]
    fn damaged_or_locked_storage_gets_bounded_generic_recovery_copy() {
        let error = anyhow::anyhow!(
            "error returned from database: (code: 11) database disk image is malformed at SELECT body_text FROM messages"
        );
        let failure = StartupStorageFailure::from_error(&error);

        assert_eq!(failure.kind, StartupStorageFailureKind::StorageUnavailable);
        assert!(failure.title.contains("safely open"));
        assert!(failure
            .message
            .contains("instead of opening an empty mailbox"));
        assert!(!failure.message.contains("database disk image"));
        assert!(!failure.message.contains("SELECT"));
        assert!(!failure.message.contains("body_text"));
    }

    #[tokio::test]
    async fn real_store_open_damage_is_classified_without_exposing_the_profile_path() {
        let directory = tempfile::tempdir().expect("temporary profile directory");
        let path = directory.path().join("private-subject-and-token.sqlite");
        std::fs::write(&path, b"not a sqlite database containing secret body text")
            .expect("write damaged fictional profile");

        let error = match dakia_core::Store::open(&path).await {
            Ok(_) => panic!("damaged profile must not open"),
            Err(error) => error,
        };
        let failure = StartupStorageFailure::from_error(&error);

        assert_eq!(failure.kind, StartupStorageFailureKind::StorageUnavailable);
        for prohibited in ["private-subject", "secret body text", "sqlite"] {
            assert!(!failure.title.contains(prohibited));
            assert!(!failure.message.contains(prohibited));
        }
    }

    #[test]
    fn production_orchestration_closes_webviews_then_waits_for_fatal_recovery_window_before_exit() {
        let error =
            anyhow::anyhow!("profile schema version 9 is newer than this Dakia build supports (1)");
        let exited = Arc::new(AtomicBool::new(false));
        let callback_exited = exited.clone();
        let mut presenter = RecordingPresenter::default();

        let kind = present_startup_storage_failure(&mut presenter, &error, move || {
            callback_exited.store(true, Ordering::SeqCst);
        });

        assert_eq!(kind, StartupStorageFailureKind::NewerProfile);
        assert_eq!(
            presenter.events,
            ["isolated-mailbox-surfaces", "showed-fatal-recovery-window"]
        );
        assert!(!exited.load(Ordering::SeqCst));
        presenter
            .on_closed
            .take()
            .expect("fatal recovery callback is retained until the user closes the window")(
        );
        assert!(exited.load(Ordering::SeqCst));
    }

    #[test]
    fn production_surface_isolation_removes_webviews_but_keeps_native_windows_alive() {
        let app = tauri::test::mock_builder()
            .build(crate::test_support::mock_context())
            .expect("build production-shaped Tauri app");
        let surface = tauri::WebviewWindowBuilder::new(
            &app,
            "startup-failure-surface",
            tauri::WebviewUrl::default(),
        )
        .build()
        .expect("build mailbox surface");

        let native_window =
            AsRef::<tauri::Webview<tauri::test::MockRuntime>>::as_ref(&surface).window();
        assert!(app.get_webview_window("startup-failure-surface").is_some());
        assert!(isolate_mailbox_surfaces(&app));

        // MockRuntime does not model the visibility transition, but the real
        // hide call above returned successfully. A usable Window handle after
        // Webview removal proves the native keeper was not destroyed.
        assert!(native_window.is_visible().is_ok());
        assert!(app.get_webview_window("startup-failure-surface").is_none());
        assert_eq!(native_window.label(), "startup-failure-surface");
        drop(surface);
    }

    #[test]
    fn failed_surface_isolation_exits_without_scheduling_a_recovery_window() {
        let error = anyhow::anyhow!("database disk image is malformed");
        let exited = Arc::new(AtomicBool::new(false));
        let callback_exited = exited.clone();
        let mut presenter = RecordingPresenter {
            isolation_succeeds: false,
            ..RecordingPresenter::default()
        };

        present_startup_storage_failure(&mut presenter, &error, move || {
            callback_exited.store(true, Ordering::SeqCst);
        });

        assert_eq!(presenter.events, ["isolated-mailbox-surfaces"]);
        assert!(presenter.failure.is_none());
        assert!(presenter.on_closed.is_none());
        assert!(exited.load(Ordering::SeqCst));
    }

    #[test]
    fn fatal_recovery_paths_are_enum_only_and_never_contain_raw_error_details() {
        let cases = [
            (
                anyhow::anyhow!(
                    "profile schema version 9 is newer than this Dakia build supports (1)"
                ),
                "/fatal-recovery.html?reason=newer_profile",
            ),
            (
                anyhow::anyhow!("profile schema validation failed: private-token"),
                "/fatal-recovery.html?reason=upgrade_failed",
            ),
            (
                anyhow::anyhow!("database disk image is malformed at /private/secret.sqlite"),
                "/fatal-recovery.html?reason=storage_unavailable",
            ),
        ];

        for (error, expected_path) in cases {
            let failure = StartupStorageFailure::from_error(&error);
            assert_eq!(failure.fatal_recovery_path(), expected_path);
            assert!(!failure.fatal_recovery_path().contains("private"));
            assert!(!failure.fatal_recovery_path().contains("token"));
        }
    }

    #[test]
    fn repeated_fatal_window_close_events_exit_once() {
        let exits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let callback_exits = exits.clone();
        let callback = Arc::new(Mutex::new(Some(Box::new(move || {
            callback_exits.fetch_add(1, Ordering::SeqCst);
        }) as Box<dyn FnOnce() + Send>)));

        run_startup_failure_exit(&callback);
        run_startup_failure_exit(&callback);

        assert_eq!(exits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn fatal_window_build_failure_runs_the_exit_callback_and_keeps_the_cli_exit_nonzero() {
        let exit_status = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let callback_status = exit_status.clone();
        let callback = Arc::new(Mutex::new(Some(Box::new(move || {
            callback_status.store(true, Ordering::SeqCst);
        }) as Box<dyn FnOnce() + Send>)));

        // This is the same fail-closed callback branch used after a
        // WebviewWindowBuilder error. It must not leave the startup process
        // waiting for a page that never opened.
        run_startup_failure_exit(&callback);

        assert!(exit_status.load(Ordering::SeqCst));
        assert_eq!(startup_storage_failure_exit_code(), 1);
    }

    #[test]
    fn startup_storage_failure_uses_a_nonzero_exit_code() {
        assert_eq!(startup_storage_failure_exit_code(), 1);
    }
}
