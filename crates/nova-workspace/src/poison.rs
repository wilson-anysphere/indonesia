#[track_caller]
fn log_poisoned_lock<E: std::fmt::Display>(kind: &'static str, err: E) {
    let loc = std::panic::Location::caller();
    tracing::error!(
      target = "nova.workspace",
      kind,
      file = loc.file(),
      line = loc.line(),
      column = loc.column(),
      error = %err,
      "poisoned; continuing with recovered guard"
    );
}

pub(crate) trait RecoverPoisoned<T> {
    fn recover_poisoned(self) -> T;
}

impl<T> RecoverPoisoned<T> for Result<T, std::sync::PoisonError<T>> {
    #[track_caller]
    fn recover_poisoned(self) -> T {
        match self {
            Ok(value) => value,
            Err(err) => {
                log_poisoned_lock("lock", &err);
                err.into_inner()
            }
        }
    }
}
