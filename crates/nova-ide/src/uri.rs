use std::path::Path;
use std::str::FromStr;
use std::sync::OnceLock;

use lsp_types::Uri;
use nova_core::{path_to_file_uri, AbsPathBuf};

static URI_FROM_PATH_ERROR_LOGGED: OnceLock<()> = OnceLock::new();

#[inline]
pub(crate) fn uri_from_path_best_effort(path: &Path, context: &'static str) -> Option<Uri> {
    let abs = match AbsPathBuf::new(path.to_path_buf()) {
        Ok(abs) => abs,
        Err(err) => {
            log_uri_from_path_error_once(path, context, "failed to create absolute path", &err);
            return None;
        }
    };

    let uri = match path_to_file_uri(&abs) {
        Ok(uri) => uri,
        Err(err) => {
            log_uri_from_path_error_once(path, context, "failed to convert path to file URI", &err);
            return None;
        }
    };

    match Uri::from_str(&uri) {
        Ok(uri) => Some(uri),
        Err(err) => {
            log_uri_from_path_error_once(path, context, "failed to parse file URI", &err);
            None
        }
    }
}

fn log_uri_from_path_error_once<E: std::fmt::Debug>(
    path: &Path,
    context: &'static str,
    detail: &'static str,
    err: &E,
) {
    if URI_FROM_PATH_ERROR_LOGGED.set(()).is_ok() {
        tracing::debug!(
          target = "nova.ide",
          context,
          path = %path.display(),
          detail,
          error = ?err,
          "failed to convert path to LSP URI (best effort)"
        );
    }
}
