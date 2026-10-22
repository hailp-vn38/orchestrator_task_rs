//! Module dùng chung cho các example của `orchestrator_task_rs`.
//! Tất cả các example cùng trỏ về 1 thư mục `target/homelab_shared_workspace`
//! (qua `WorkspaceLayout`) và dùng chung bộ hiển thị stream sự kiện trực tiếp.

pub mod env;
pub mod pdf;
pub mod prompts;
pub mod runtime;
pub mod stream;
pub mod tools;
pub mod ytdlp;
