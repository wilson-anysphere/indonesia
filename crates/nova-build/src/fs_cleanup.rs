use std::panic::Location;
use std::path::Path;
#[cfg(unix)]
use std::sync::OnceLock;

#[track_caller]
pub(crate) fn remove_file_best_effort(path: &Path, reason: &'static str) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            let loc = Location::caller();
            tracing::debug!(
              target = "nova.build",
              path = %path.display(),
              reason,
              file = loc.file(),
              line = loc.line(),
              column = loc.column(),
              error = %err,
              "failed to remove file (best effort)"
            );
        }
    }
}

#[cfg(unix)]
static SYNC_DIR_ERROR_LOGGED: OnceLock<()> = OnceLock::new();

#[track_caller]
pub(crate) fn sync_dir_best_effort(dir: &Path, reason: &'static str) {
    #[cfg(unix)]
    {
        match std::fs::File::open(dir).and_then(|dir| dir.sync_all()) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                if SYNC_DIR_ERROR_LOGGED.set(()).is_ok() {
                    let loc = Location::caller();
                    tracing::debug!(
                      target = "nova.build",
                      path = %dir.display(),
                      reason,
                      file = loc.file(),
                      line = loc.line(),
                      column = loc.column(),
                      error = %err,
                      "failed to sync directory (best effort)"
                    );
                }
            }
        }
    }

    #[cfg(not(unix))]
    {
        let _ = (dir, reason);
    }
}
