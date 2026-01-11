use thiserror::Error;

pub type DebugResult<T> = Result<T, DebugError>;

#[derive(Error, Debug)]
pub enum DebugError {
    #[error("jdwp: {0}")]
    Jdwp(#[from] nova_jdwp::JdwpError),
    #[error("unknown variablesReference {0}")]
    UnknownVariablesReference(i64),
    #[error("unknown object handle {0}")]
    UnknownObjectHandle(i64),
}
