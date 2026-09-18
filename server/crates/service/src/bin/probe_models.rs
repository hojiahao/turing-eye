use std::process::ExitCode;

use tokio::sync::watch;
use turing_eye_service::infrastructure::models::probe;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    if std::env::var_os("R1_ALLOW_LIVE_PROBES").as_deref() != Some(std::ffi::OsStr::new("1")) {
        eprintln!("R1_MODEL_PROBE_LIVE_PERMISSION_REQUIRED");
        return ExitCode::from(2);
    }
    let (cancel, cancellation) = watch::channel(false);
    let signal = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = cancel.send(true);
        }
        // Keep the sender alive until the task is aborted, even if signal setup failed.
        std::future::pending::<()>().await;
    });
    let result = probe::run_from_env(cancellation).await;
    signal.abort();
    let _ = signal.await;
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(_) => {
            eprintln!("R1_MODEL_PROBE_OUTPUT_ERROR");
            ExitCode::from(2)
        }
    }
}
