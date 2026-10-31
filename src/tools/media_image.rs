//! Công cụ tạo hình ảnh / sơ đồ (`image_create`) và trait [`ImageGeneratorBackend`] cho `Orchestrator`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use llm_wiki::slugify;
use orchestrator_agent_rs::{
    ArtifactInput, HostTool, ToolContext, ToolDescriptor, ToolEffect, ToolError, ToolOutcome,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::tools::media_artifacts::{
    mirror_artifact_to_dir, parse_role, DEFAULT_PNG_HEIGHT, DEFAULT_PNG_WIDTH, DEFAULT_SVG_HEIGHT,
    DEFAULT_SVG_WIDTH, IMAGE_CREATE_TIMEOUT,
};
#[cfg(test)]
pub(crate) use crate::tools::media_image_codecs::encode_base64;
pub(crate) use crate::tools::media_image_codecs::{
    encode_gradient_png, ensure_svg_root_dimensions, extract_svg_root_dimensions, render_svg_card,
    resolve_first_frame_from_args, resolve_image_references_from_args,
};
use crate::tools::media_video::MediaCallContext;
use crate::tools::wiki_bridge::{host_tool_err, host_tool_outcome};

/// Ảnh tham chiếu đã được xác minh toàn vẹn để chuyển xuống Backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferenceImageInput {
    pub bytes: Vec<u8>,
    pub mime_type: String,
}

/// Yêu cầu sinh hình ảnh gửi tới [`ImageGeneratorBackend`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageGenerationRequest {
    pub prompt: String,
    pub title: Option<String>,
    pub filename: String,
    /// `"png"` hoặc `"svg"` (mặc định `"png"`).
    pub format: String,
    pub width: u32,
    pub height: u32,
    pub svg_content: Option<String>,
    #[serde(default)]
    pub reference_images: Vec<ReferenceImageInput>,
    #[serde(default)]
    pub transparent_background: bool,
}

impl ImageGenerationRequest {
    /// Kiểm tra yêu cầu này có xuất ra định dạng vector SVG (`image/svg+xml`) hay không.
    pub fn is_svg(&self) -> bool {
        self.format.trim().eq_ignore_ascii_case("svg")
            || self
                .svg_content
                .as_deref()
                .is_some_and(|s| !s.trim().is_empty())
    }

    /// Trả về kích thước hiệu lực `(width, height)` đã chuẩn hóa trong khoảng `16..=2048`:
    /// - Nếu `width` / `height` khác `0`, giữ giá trị do Agent chỉ định (clamp `16..=2048`).
    /// - Nếu là SVG có truyền `<svg ...>` đầy đủ trong `svg_content` và `width == 0` hoặc `height == 0`,
    ///   ưu tiên trích xuất kích thước từ thuộc tính `width`/`height` hoặc `viewBox` của thẻ `<svg>` gốc.
    /// - Ngược lại, dùng mặc định theo định dạng (`1200x675` cho SVG, `1024x1024` cho PNG).
    pub fn effective_dimensions(&self) -> (u32, u32) {
        let is_svg = self.is_svg();
        let svg_dims = if is_svg {
            self.svg_content
                .as_deref()
                .and_then(extract_svg_root_dimensions)
        } else {
            None
        };
        let (default_w, default_h) = if is_svg {
            svg_dims.unwrap_or((DEFAULT_SVG_WIDTH, DEFAULT_SVG_HEIGHT))
        } else {
            (DEFAULT_PNG_WIDTH, DEFAULT_PNG_HEIGHT)
        };
        let width = if self.width == 0 {
            default_w
        } else {
            self.width
        }
        .clamp(16, 2048);
        let height = if self.height == 0 {
            default_h
        } else {
            self.height
        }
        .clamp(16, 2048);
        (width, height)
    }
}

/// Kết quả hình ảnh sinh ra từ [`ImageGeneratorBackend`].
#[derive(Debug, Clone)]
pub struct GeneratedImagePayload {
    pub filename: String,
    pub mime_type: String,
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub provider: String,
}

/// Trait mở rộng cho phép Host cắm bất kỳ engine tạo ảnh nào (OpenAI Images, ComfyUI, SDXL, Flux, hoặc Local Renderer).
#[async_trait::async_trait]
pub trait ImageGeneratorBackend: Send + Sync {
    async fn generate_image(
        &self,
        req: &ImageGenerationRequest,
        ctx: &MediaCallContext,
    ) -> Result<GeneratedImagePayload, String>;
}

/// Backend mặc định tự chủ (không phụ thuộc GPU rời): hỗ trợ kết xuất ảnh PNG chuẩn (binary RIFF/IDAT + CRC32)
/// và đồ họa vector SVG (`image/svg+xml`) trực tiếp từ `prompt` hoặc `svg_content` của Subagent.
#[derive(Debug, Clone, Default)]
pub struct DefaultImageGeneratorBackend;

#[async_trait::async_trait]
impl ImageGeneratorBackend for DefaultImageGeneratorBackend {
    async fn generate_image(
        &self,
        req: &ImageGenerationRequest,
        _ctx: &MediaCallContext,
    ) -> Result<GeneratedImagePayload, String> {
        let (width, height) = req.effective_dimensions();

        if req.is_svg() {
            let (svg, final_w, final_h) = match req.svg_content {
                Some(ref raw)
                    if raw
                        .trim()
                        .get(..4)
                        .is_some_and(|p| p.eq_ignore_ascii_case("<svg")) =>
                {
                    ensure_svg_root_dimensions(raw.trim(), width, height)
                }
                Some(ref inner) => (
                    render_svg_card(
                        width,
                        height,
                        req.title.as_deref().unwrap_or("Generated Diagram"),
                        &req.prompt,
                        Some(inner),
                    ),
                    width,
                    height,
                ),
                None => (
                    render_svg_card(
                        width,
                        height,
                        req.title.as_deref().unwrap_or("Visual Summary"),
                        &req.prompt,
                        None,
                    ),
                    width,
                    height,
                ),
            };
            let stem = slugify(
                req.filename
                    .trim_end_matches(".svg")
                    .trim_end_matches(".png"),
                64,
            );
            let safe_name = format!("{}.svg", if stem.is_empty() { "image" } else { &stem });
            return Ok(GeneratedImagePayload {
                filename: safe_name,
                mime_type: "image/svg+xml".to_string(),
                bytes: svg.into_bytes(),
                width: final_w,
                height: final_h,
                provider: "builtin_svg_renderer".to_string(),
            });
        }

        // Mặc định: kết xuất ảnh PNG chuẩn RFC-2083 với đúng kích thước (width, height) trong IHDR
        let png_bytes = encode_gradient_png(width, height, &req.prompt);
        let stem = slugify(
            req.filename
                .trim_end_matches(".png")
                .trim_end_matches(".svg"),
            64,
        );
        let safe_name = format!("{}.png", if stem.is_empty() { "image" } else { &stem });
        Ok(GeneratedImagePayload {
            filename: safe_name,
            mime_type: "image/png".to_string(),
            bytes: png_bytes,
            width,
            height,
            provider: "builtin_png_renderer".to_string(),
        })
    }
}

/// Công cụ `image_create`: tạo hình ảnh minh họa / sơ đồ (`image/png` hoặc `image/svg+xml`)
/// và lưu trữ vào `ArtifactStore` của Task.
pub struct ImageCreateTool {
    pub backend: Arc<dyn ImageGeneratorBackend>,
    pub output_dir: Option<PathBuf>,
}

impl ImageCreateTool {
    pub fn new(backend: Arc<dyn ImageGeneratorBackend>) -> Self {
        Self {
            backend,
            output_dir: None,
        }
    }

    pub fn with_output_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.output_dir = Some(dir.into());
        self
    }

    pub fn into_host_tool(self) -> Arc<dyn HostTool> {
        Arc::new(self)
    }
}

impl Default for ImageCreateTool {
    fn default() -> Self {
        Self::new(Arc::new(DefaultImageGeneratorBackend))
    }
}

#[async_trait::async_trait]
impl HostTool for ImageCreateTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "image_create".to_string(),
            description: "Generate an illustration image or diagram (PNG or SVG) and publish it as a Task Artifact.".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["filename", "prompt"],
                "properties": {
                    "filename": {
                        "type": "string",
                        "description": "Output image filename (e.g. 'architecture-diagram.svg' or 'cover-banner.png')."
                    },
                    "prompt": {
                        "type": "string",
                        "description": "Detailed visual prompt or description of the image/diagram to generate."
                    },
                    "title": {
                        "type": "string",
                        "description": "Optional heading/caption displayed on the visual card or diagram."
                    },
                    "format": {
                        "type": "string",
                        "enum": ["png", "svg"],
                        "description": "Image format: 'png' (raster PNG) or 'svg' (vector diagram). Defaults to extension or 'png'."
                    },
                    "width": {
                        "type": "integer",
                        "description": "Optional image width in pixels (16..=2048). Agent may set custom width or omit to use format default (1024 for PNG, 1200 for SVG)."
                    },
                    "height": {
                        "type": "integer",
                        "description": "Optional image height in pixels (16..=2048). Agent may set custom height or omit to use format default (1024 for PNG, 675 for SVG)."
                    },
                    "svg_content": {
                        "type": "string",
                        "description": "Optional raw SVG markup (<svg ...>...</svg>) when authoring custom vector diagrams."
                    },
                    "reference_artifact_ids": {
                        "type": "array",
                        "items": { "type": "string" },
                        "maxItems": 4,
                        "description": "Optional list of up to 4 image Artifact IDs in the current task to use as visual references."
                    },
                    "reference_images": {
                        "type": "array",
                        "items": { "type": "string" },
                        "maxItems": 4,
                        "description": "Optional list of up to 4 reference images as data:image/...;base64,... strings."
                    },
                    "transparent_background": {
                        "type": "boolean",
                        "description": "Optional flag to request an isolated subject with a transparent PNG background."
                    },
                    "role": {
                        "type": "string",
                        "enum": ["primary", "intermediate"],
                        "description": "Artifact visibility role (default: 'primary')."
                    }
                },
                "additionalProperties": false
            }),
            effect: ToolEffect::Mutating,
        }
    }

    fn timeout(&self) -> Option<Duration> {
        Some(IMAGE_CREATE_TIMEOUT)
    }

    async fn execute(&self, ctx: ToolContext, args: Value) -> Result<ToolOutcome, ToolError> {
        let filename = args
            .get("filename")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| host_tool_err("INVALID_ARGUMENT", "Missing filename"))?;
        let prompt = args
            .get("prompt")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| host_tool_err("INVALID_ARGUMENT", "Missing prompt"))?;
        let title = args
            .get("title")
            .and_then(Value::as_str)
            .map(str::to_string);
        let svg_content = args
            .get("svg_content")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string);
        let inferred_fmt =
            if filename.trim().to_ascii_lowercase().ends_with(".svg") || svg_content.is_some() {
                "svg"
            } else {
                "png"
            };
        let format = args
            .get("format")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(inferred_fmt)
            .to_ascii_lowercase();
        let raw_width = args
            .get("width")
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(0);
        let raw_height = args
            .get("height")
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(0);
        let transparent_background = args
            .get("transparent_background")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let role = parse_role(args.get("role").and_then(Value::as_str));
        let reference_images = resolve_image_references_from_args(Some(&ctx), &args).await?;

        let mut req = ImageGenerationRequest {
            prompt: prompt.to_string(),
            title,
            filename: filename.to_string(),
            format,
            width: raw_width,
            height: raw_height,
            svg_content,
            reference_images,
            transparent_background,
        };
        let (width, height) = req.effective_dimensions();
        req.width = width;
        req.height = height;

        let call_ctx = MediaCallContext {
            cancel: ctx.cancel_signal().clone(),
        };
        let generated = tokio::select! {
            result = self.backend.generate_image(&req, &call_ctx) => {
                result.map_err(|e| host_tool_err("IMAGE_GENERATION_FAILED", e))?
            }
            () = ctx.cancel_token().cancelled() => {
                return Err(host_tool_err(
                    "CANCELLED",
                    "image generation was cancelled before the provider finished",
                ));
            }
        };

        let mirrored_path = mirror_artifact_to_dir(
            self.output_dir.as_ref(),
            &generated.filename,
            &generated.bytes,
        );

        let artifact = ctx
            .artifacts()
            .put(ArtifactInput::from_bytes(
                &generated.filename,
                &generated.mime_type,
                role,
                generated.bytes,
            ))
            .await?;

        Ok(host_tool_outcome(
            &ctx,
            format!(
                "Created image artifact '{}' (id={}, mime={}, width={}, height={}, provider={}, bytes={})",
                artifact.name,
                artifact.id,
                artifact.mime_type,
                generated.width,
                generated.height,
                generated.provider,
                artifact.size_bytes
            ),
            json!({
                "artifact_id": artifact.id,
                "artifact_ids": [artifact.id],
                "artifact_kind": "image",
                "filename": artifact.name,
                "mime_type": artifact.mime_type,
                "size_bytes": artifact.size_bytes,
                "sha256": artifact.sha256,
                "width": generated.width,
                "height": generated.height,
                "provider": generated.provider,
                "path": mirrored_path
            }),
        ))
    }
}
