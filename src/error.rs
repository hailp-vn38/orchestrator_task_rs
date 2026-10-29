//! Typed errors cho `orchestrator_flow_rs`.

use std::fmt;

use orchestrator_agent_rs::OrchestratorError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    InvalidConfig,
    InvalidInput,
    NotFound,
    PermissionDenied,
    SessionBusy,
    StoreAlreadyOwned,
    StoreError,
    OrchestratorError,
    NotReady,
    ShuttingDown,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidConfig => "INVALID_CONFIG",
            Self::InvalidInput => "INVALID_INPUT",
            Self::NotFound => "NOT_FOUND",
            Self::PermissionDenied => "PERMISSION_DENIED",
            Self::SessionBusy => "SESSION_BUSY",
            Self::StoreAlreadyOwned => "STORE_ALREADY_OWNED",
            Self::StoreError => "STORE_ERROR",
            Self::OrchestratorError => "ORCHESTRATOR_ERROR",
            Self::NotReady => "NOT_READY",
            Self::ShuttingDown => "SHUTTING_DOWN",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskError {
    code: ErrorCode,
    message: String,
}

impl TaskError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn config(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidConfig, message)
    }

    pub fn invalid_input(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidInput, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::NotFound, message)
    }

    pub fn storage(op: &str, err: impl fmt::Display) -> Self {
        Self::new(ErrorCode::StoreError, format!("{op}: {err}"))
    }

    pub fn code(&self) -> ErrorCode {
        self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for TaskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for TaskError {}

impl From<OrchestratorError> for TaskError {
    fn from(err: OrchestratorError) -> Self {
        Self::new(ErrorCode::OrchestratorError, err.to_string())
    }
}

pub type TaskResult<T> = Result<T, TaskError>;
