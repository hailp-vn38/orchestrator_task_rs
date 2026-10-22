//! Re-export bộ công cụ chuyển đổi PDF (`anydoc`) tích hợp sẵn từ crate `orchestrator_task_rs`.

#[allow(unused_imports)]
pub use orchestrator_task_rs::{
    build_sample_technical_pdf, convert_document_with_anydoc, pdf_host_tools, AnydocInspectPdfTool,
    AnydocSaveToSourcesTool, ConvertedPdfDoc, SharedPdfState,
};
