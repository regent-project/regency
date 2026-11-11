//! Process entry of every service (D22): a console process everywhere, or a
//! Windows service under the Service Control Manager (`--windows-service`,
//! set by the MSI's service registration).
//!
//! Under the SCM, Stop and Shutdown requests take the same graceful path as
//! SIGTERM ([`crate::telemetry::request_shutdown`]); a non-zero exit status
//! is reported as a service-specific error, so the service's recovery actions
//! (restart) apply, e.g. after a D21 configuration change (status 3).

/// The service's main, returning the process exit status.
pub type Main = Box<dyn FnOnce() -> i32 + Send + 'static>;

/// Run `main` and exit with its status. `windows_service` is ignored on
/// other systems.
pub fn launch(service_name: &str, windows_service: bool, main: Main) -> ! {
    #[cfg(windows)]
    if windows_service {
        std::process::exit(windows::run(service_name, main));
    }
    let _ = (service_name, windows_service);
    std::process::exit(main())
}

/// The process's state directory: `/var/lib/regency/<service>` on Unix,
/// `%ProgramData%\regency\<service>` on Windows.
pub fn default_state_dir(service: &str) -> String {
    #[cfg(windows)]
    {
        let base = std::env::var("ProgramData").unwrap_or_else(|_| r"C:\ProgramData".to_string());
        format!(r"{base}\regency\{service}")
    }
    #[cfg(not(windows))]
    {
        format!("/var/lib/regency/{service}")
    }
}

#[cfg(windows)]
mod windows {
    use std::ffi::OsString;
    use std::sync::Mutex;
    use std::time::Duration;

    use windows_service::service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
        ServiceType,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
    use windows_service::{define_windows_service, service_dispatcher};

    use super::Main;

    static NAME: Mutex<String> = Mutex::new(String::new());
    static MAIN: Mutex<Option<Main>> = Mutex::new(None);
    static CODE: Mutex<i32> = Mutex::new(0);

    define_windows_service!(ffi_service_main, service_main);

    pub fn run(name: &str, main: Main) -> i32 {
        *NAME.lock().expect("name") = name.to_string();
        *MAIN.lock().expect("main") = Some(main);
        if let Err(e) = service_dispatcher::start(name, ffi_service_main) {
            eprintln!("{name}: cannot start as a Windows service: {e}");
            return 1;
        }
        *CODE.lock().expect("code")
    }

    fn status(state: ServiceState, code: i32) -> ServiceStatus {
        ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: if state == ServiceState::Running {
                ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN
            } else {
                ServiceControlAccept::empty()
            },
            exit_code: if code == 0 {
                ServiceExitCode::NO_ERROR
            } else {
                ServiceExitCode::ServiceSpecific(code as u32)
            },
            checkpoint: 0,
            wait_hint: if state == ServiceState::StopPending {
                Duration::from_secs(120)
            } else {
                Duration::ZERO
            },
            process_id: None,
        }
    }

    fn service_main(_arguments: Vec<OsString>) {
        let name = NAME.lock().expect("name").clone();
        let Some(main) = MAIN.lock().expect("main").take() else {
            return;
        };
        let handle: std::sync::Arc<Mutex<Option<service_control_handler::ServiceStatusHandle>>> =
            std::sync::Arc::new(Mutex::new(None));
        let for_handler = handle.clone();
        let registered = service_control_handler::register(&name, move |control| match control {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                if let Some(handle) = *for_handler.lock().expect("handle") {
                    let _ = handle.set_service_status(status(ServiceState::StopPending, 0));
                }
                crate::telemetry::request_shutdown();
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        });
        let Ok(status_handle) = registered else {
            return;
        };
        *handle.lock().expect("handle") = Some(status_handle);
        let _ = status_handle.set_service_status(status(ServiceState::Running, 0));
        let code = main();
        if code != 0 {
            // an unexpected termination for the SCM: its recovery actions
            // (restart, set by the MSI) apply without further flags
            std::process::exit(code);
        }
        *CODE.lock().expect("code") = code;
        let _ = status_handle.set_service_status(status(ServiceState::Stopped, 0));
    }
}
