//! Công cụ `HostTool` `asr_transcribe` và Seam [`AsrTranscriberBackend`] cho phép Subagent
//! (`media_creator`) phiên âm các file âm thanh `.wav` trong `ArtifactStore` của Task.

use std::sync::Arc;

use orchestrator_agent_rs::{
    HostTool, ToolContext, ToolDescriptor, ToolEffect, ToolError, ToolOutcome,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::tools::wiki_bridge::{host_tool_err, host_tool_outcome};

/// Tên công cụ phiên âm file `.wav` trong `ArtifactStore`.
pub const ASR_TRANSCRIBE_TOOL: &str = "asr_transcribe";

/// Mã lỗi ổn định khi artifact âm thanh không phải định dạng `.wav` hợp lệ.
pub const CODE_ASR_UNSUPPORTED_AUDIO: &str = "ASR_UNSUPPORTED_AUDIO";

/// Định dạng mẫu âm thanh bên trong container `.wav`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WavSampleFormat {
    Int,
    Float,
}

/// Thông tin header `.wav` đã xác thực qua [`inspect_wav_bytes`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WavHeaderInfo {
    pub format: WavSampleFormat,
    pub sample_rate: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
    pub data_bytes: u64,
    pub duration_ms: u64,
}

const KSDATAFORMAT_SUBTYPE_TAIL: [u8; 14] = [
    0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71,
];

/// Kiểm tra và trích xuất thông tin header của dữ liệu `.wav` (`RIFF....WAVE`).
///
/// Hỗ trợ:
/// - `WAVE_FORMAT_PCM` (`0x0001`): `Int` 8/16/24/32-bit
/// - `WAVE_FORMAT_IEEE_FLOAT` (`0x0003`): `Float` 32-bit
/// - `WAVE_FORMAT_EXTENSIBLE` (`0xFFFE`): ánh xạ theo sub-format PCM (`0x0001`) hoặc IEEE Float (`0x0003`)
/// - Bỏ qua các chunk phụ (`LIST`, `JUNK`, `fact`, `bext`...) và xử lý byte đệm cho chunk có kích thước lẻ.
pub fn inspect_wav_bytes(bytes: &[u8]) -> Result<WavHeaderInfo, String> {
    if bytes.len() < 12 {
        return Err("file is shorter than 12-byte RIFF/WAVE header".to_string());
    }
    if &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("missing RIFF/WAVE signature".to_string());
    }

    let mut offset = 12usize;
    let mut fmt_info: Option<(WavSampleFormat, u32, u16, u16)> = None;
    let mut data_bytes: Option<u64> = None;

    while offset + 8 <= bytes.len() {
        let chunk_id = &bytes[offset..offset + 4];
        let chunk_size = u32::from_le_bytes([
            bytes[offset + 4],
            bytes[offset + 5],
            bytes[offset + 6],
            bytes[offset + 7],
        ]) as usize;
        let payload_start = offset + 8;
        let payload_end = payload_start
            .checked_add(chunk_size)
            .ok_or_else(|| "WAV chunk size overflow".to_string())?;
        if payload_end > bytes.len() {
            return Err(format!(
                "truncated WAV chunk '{}' (declared {} bytes, remaining {} bytes)",
                String::from_utf8_lossy(chunk_id),
                chunk_size,
                bytes.len().saturating_sub(payload_start)
            ));
        }

        if chunk_id == b"fmt " {
            if chunk_size < 16 {
                return Err("WAV fmt chunk is shorter than 16 bytes".to_string());
            }
            let fmt = &bytes[payload_start..payload_end];
            let raw_format = u16::from_le_bytes([fmt[0], fmt[1]]);
            let channels = u16::from_le_bytes([fmt[2], fmt[3]]);
            let sample_rate = u32::from_le_bytes([fmt[4], fmt[5], fmt[6], fmt[7]]);
            let bits_per_sample = u16::from_le_bytes([fmt[14], fmt[15]]);

            if !(1..=8).contains(&channels) {
                return Err(format!(
                    "unsupported WAV channel count: {channels} (expected 1..=8)"
                ));
            }
            if !(8_000..=192_000).contains(&sample_rate) {
                return Err(format!(
                    "unsupported WAV sample rate: {sample_rate} Hz (expected 8000..=192000)"
                ));
            }

            let effective_code = if raw_format == 0xFFFE {
                if chunk_size < 40 {
                    return Err(
                        "WAVE_FORMAT_EXTENSIBLE fmt chunk must be at least 40 bytes".to_string()
                    );
                }
                if fmt[26..40] != KSDATAFORMAT_SUBTYPE_TAIL {
                    return Err("unsupported WAVE_FORMAT_EXTENSIBLE SubFormat GUID".to_string());
                }
                u16::from_le_bytes([fmt[24], fmt[25]])
            } else {
                raw_format
            };

            let sample_format = match effective_code {
                1 => {
                    if !matches!(bits_per_sample, 8 | 16 | 24 | 32) {
                        return Err(format!(
                            "unsupported PCM bits_per_sample: {bits_per_sample} (expected 8, 16, 24, or 32)"
                        ));
                    }
                    WavSampleFormat::Int
                }
                3 => {
                    if bits_per_sample != 32 {
                        return Err(format!(
                            "unsupported IEEE float bits_per_sample: {bits_per_sample} (expected 32)"
                        ));
                    }
                    WavSampleFormat::Float
                }
                other => {
                    return Err(format!("unsupported WAV audio format code: 0x{other:04x}"));
                }
            };

            fmt_info = Some((sample_format, sample_rate, channels, bits_per_sample));
        } else if chunk_id == b"data" {
            data_bytes = Some(chunk_size as u64);
        }

        let padded_size = chunk_size + (chunk_size & 1);
        offset = match payload_start.checked_add(padded_size) {
            Some(next) => next,
            None => break,
        };
    }

    let (format, sample_rate, channels, bits_per_sample) =
        fmt_info.ok_or_else(|| "WAV file is missing fmt chunk".to_string())?;
    let data_bytes = data_bytes.ok_or_else(|| "WAV file is missing data chunk".to_string())?;
    if data_bytes == 0 {
        return Err("WAV data chunk is empty".to_string());
    }

    let bytes_per_sample = (bits_per_sample / 8) as u64;
    let bytes_per_frame = (channels as u64).saturating_mul(bytes_per_sample);
    if bytes_per_frame == 0 || data_bytes < bytes_per_frame {
        return Err("WAV data chunk contains fewer than 1 audio frame".to_string());
    }

    let total_frames = data_bytes / bytes_per_frame;
    let duration_ms = (total_frames.saturating_mul(1_000)) / (sample_rate as u64);

    Ok(WavHeaderInfo {
        format,
        sample_rate,
        channels,
        bits_per_sample,
        data_bytes,
        duration_ms,
    })
}

/// Yêu cầu phiên âm gửi tới [`AsrTranscriberBackend`].
#[derive(Debug, Clone)]
pub struct AsrTranscriptionRequest {
    pub wav_bytes: Vec<u8>,
    pub header: WavHeaderInfo,
    pub language: Option<String>,
}

/// Kết quả phiên âm trả về từ [`AsrTranscriberBackend`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscribedAudioPayload {
    pub text: String,
    pub language: Option<String>,
    pub duration_ms: u64,
    pub sample_rate: u32,
    pub channels: u16,
    pub provider_id: Option<String>,
    pub model: Option<String>,
}

/// Seam cho phép Host cắm engine ASR thực tế (Sherpa-ONNX Zipformer/Gipformer hoặc Cloud Audio ASR).
#[async_trait::async_trait]
pub trait AsrTranscriberBackend: Send + Sync {
    async fn transcribe_asr(
        &self,
        request: &AsrTranscriptionRequest,
    ) -> Result<TranscribedAudioPayload, String>;
}

/// Adapter 1 (mặc định cho kiểm thử độc lập tại `orchestrator_task_rs`):
/// trả về [`TranscribedAudioPayload`] tất định dựa trên [`WavHeaderInfo`].
#[derive(Debug, Clone, Default)]
pub struct DefaultAsrTranscriberBackend;

#[async_trait::async_trait]
impl AsrTranscriberBackend for DefaultAsrTranscriberBackend {
    async fn transcribe_asr(
        &self,
        request: &AsrTranscriptionRequest,
    ) -> Result<TranscribedAudioPayload, String> {
        let lang = request
            .language
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("vi")
            .to_string();
        Ok(TranscribedAudioPayload {
            text: format!(
                "[wav_transcript: {}Hz, {}ch, {}bit, {}ms, lang={}]",
                request.header.sample_rate,
                request.header.channels,
                request.header.bits_per_sample,
                request.header.duration_ms,
                lang
            ),
            language: Some(lang),
            duration_ms: request.header.duration_ms,
            sample_rate: request.header.sample_rate,
            channels: request.header.channels,
            provider_id: Some("builtin_wav_inspector".to_string()),
            model: Some("deterministic-wav-v1".to_string()),
        })
    }
}

/// Deep Module `AsrTranscribeTool` (`HostTool` tên `"asr_transcribe"`).
pub struct AsrTranscribeTool {
    pub backend: Arc<dyn AsrTranscriberBackend>,
}

impl AsrTranscribeTool {
    pub fn new(backend: Arc<dyn AsrTranscriberBackend>) -> Self {
        Self { backend }
    }

    pub fn with_backend(backend: Arc<dyn AsrTranscriberBackend>) -> Self {
        Self::new(backend)
    }

    pub fn with_default_backend() -> Self {
        Self::new(Arc::new(DefaultAsrTranscriberBackend))
    }

    pub fn into_host_tool(self) -> Arc<dyn HostTool> {
        Arc::new(self)
    }
}

impl Default for AsrTranscribeTool {
    fn default() -> Self {
        Self::with_default_backend()
    }
}

#[async_trait::async_trait]
impl HostTool for AsrTranscribeTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: ASR_TRANSCRIBE_TOOL.to_string(),
            description: "Transcribe spoken audio from a `.wav` Task Artifact (`artifact_id`) in the current Task's ArtifactStore into text using the configured ASR provider.".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["artifact_id"],
                "additionalProperties": false,
                "properties": {
                    "artifact_id": {
                        "type": "string",
                        "description": "Artifact ID (e.g. 'art-...') of the .wav audio file in the current Task's ArtifactStore."
                    },
                    "language": {
                        "type": "string",
                        "description": "Optional ISO language code hint (e.g. 'vi', 'en')."
                    }
                }
            }),
            effect: ToolEffect::ReadOnly,
        }
    }

    async fn execute(&self, ctx: ToolContext, args: Value) -> Result<ToolOutcome, ToolError> {
        let artifact_id = args
            .get("artifact_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| host_tool_err("INVALID_ARGUMENT", "Missing or empty artifact_id"))?;
        let language = args
            .get("language")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        let reader = ctx.artifacts().open(artifact_id).await?;
        let wav_bytes = reader
            .read_all()
            .await
            .map_err(|e| host_tool_err("ARTIFACT_INTEGRITY", e.to_string()))?;

        let header = inspect_wav_bytes(&wav_bytes).map_err(|e| {
            host_tool_err(
                CODE_ASR_UNSUPPORTED_AUDIO,
                format!("Artifact '{artifact_id}' is not a valid .wav audio file: {e}"),
            )
        })?;

        let req = AsrTranscriptionRequest {
            wav_bytes,
            header,
            language: language.clone(),
        };
        let transcribed = self
            .backend
            .transcribe_asr(&req)
            .await
            .map_err(|e| host_tool_err("ASR_TRANSCRIPTION_FAILED", e))?;

        let payload = json!({
            "status": "ok",
            "artifact_id": artifact_id,
            "transcript_text": transcribed.text,
            "language": transcribed.language.or(language),
            "duration_ms": transcribed.duration_ms,
            "sample_rate": transcribed.sample_rate,
            "channels": transcribed.channels,
            "provider_id": transcribed.provider_id,
            "model": transcribed.model,
        });
        let content = serde_json::to_string(&payload).unwrap_or_default();

        Ok(host_tool_outcome(&ctx, content, payload))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_pcm16_wav(sample_rate: u32, channels: u16, frames: usize) -> Vec<u8> {
        let data_len = (frames * (channels as usize) * 2) as u32;
        let riff_len = 36 + data_len;
        let byte_rate = sample_rate * (channels as u32) * 2;
        let block_align = channels * 2;

        let mut buf = Vec::with_capacity((riff_len + 8) as usize);
        buf.extend_from_slice(b"RIFF");
        buf.extend_from_slice(&riff_len.to_le_bytes());
        buf.extend_from_slice(b"WAVE");
        buf.extend_from_slice(b"fmt ");
        buf.extend_from_slice(&16u32.to_le_bytes());
        buf.extend_from_slice(&1u16.to_le_bytes());
        buf.extend_from_slice(&channels.to_le_bytes());
        buf.extend_from_slice(&sample_rate.to_le_bytes());
        buf.extend_from_slice(&byte_rate.to_le_bytes());
        buf.extend_from_slice(&block_align.to_le_bytes());
        buf.extend_from_slice(&16u16.to_le_bytes());
        buf.extend_from_slice(b"data");
        buf.extend_from_slice(&data_len.to_le_bytes());
        buf.resize(buf.len() + data_len as usize, 0u8);
        buf
    }

    #[test]
    fn inspect_wav_bytes_accepts_pcm16_and_float32_and_extensible() {
        let pcm = build_pcm16_wav(16_000, 1, 16_000);
        let info = inspect_wav_bytes(&pcm).expect("valid pcm16 wav");
        assert_eq!(info.format, WavSampleFormat::Int);
        assert_eq!(info.sample_rate, 16_000);
        assert_eq!(info.channels, 1);
        assert_eq!(info.bits_per_sample, 16);
        assert_eq!(info.data_bytes, 32_000);
        assert_eq!(info.duration_ms, 1_000);

        // Float32 stereo 48kHz with an odd-sized LIST chunk before data
        let frames = 24_000usize;
        let data_len = (frames * 2 * 4) as u32;
        let mut float_wav = Vec::new();
        float_wav.extend_from_slice(b"RIFF");
        float_wav.extend_from_slice(&(44 + 8 + 6 + data_len).to_le_bytes());
        float_wav.extend_from_slice(b"WAVE");
        float_wav.extend_from_slice(b"LIST");
        float_wav.extend_from_slice(&5u32.to_le_bytes()); // odd size 5 + 1 pad byte
        float_wav.extend_from_slice(b"INFO!");
        float_wav.push(0); // pad byte
        float_wav.extend_from_slice(b"fmt ");
        float_wav.extend_from_slice(&16u32.to_le_bytes());
        float_wav.extend_from_slice(&3u16.to_le_bytes()); // IEEE float
        float_wav.extend_from_slice(&2u16.to_le_bytes());
        float_wav.extend_from_slice(&48_000u32.to_le_bytes());
        float_wav.extend_from_slice(&(48_000u32 * 8).to_le_bytes());
        float_wav.extend_from_slice(&8u16.to_le_bytes());
        float_wav.extend_from_slice(&32u16.to_le_bytes());
        float_wav.extend_from_slice(b"data");
        float_wav.extend_from_slice(&data_len.to_le_bytes());
        float_wav.resize(float_wav.len() + data_len as usize, 0u8);

        let f_info = inspect_wav_bytes(&float_wav).expect("valid float32 wav with odd LIST chunk");
        assert_eq!(f_info.format, WavSampleFormat::Float);
        assert_eq!(f_info.sample_rate, 48_000);
        assert_eq!(f_info.channels, 2);
        assert_eq!(f_info.bits_per_sample, 32);
        assert_eq!(f_info.duration_ms, 500);

        // WAVE_FORMAT_EXTENSIBLE (0xFFFE) with PCM subformat (1)
        let mut ext_wav = Vec::new();
        ext_wav.extend_from_slice(b"RIFF");
        ext_wav.extend_from_slice(&100u32.to_le_bytes());
        ext_wav.extend_from_slice(b"WAVE");
        ext_wav.extend_from_slice(b"fmt ");
        ext_wav.extend_from_slice(&40u32.to_le_bytes());
        ext_wav.extend_from_slice(&0xFFFEu16.to_le_bytes());
        ext_wav.extend_from_slice(&1u16.to_le_bytes());
        ext_wav.extend_from_slice(&24_000u32.to_le_bytes());
        ext_wav.extend_from_slice(&48_000u32.to_le_bytes());
        ext_wav.extend_from_slice(&2u16.to_le_bytes());
        ext_wav.extend_from_slice(&16u16.to_le_bytes());
        ext_wav.extend_from_slice(&22u16.to_le_bytes()); // cbSize
        ext_wav.extend_from_slice(&16u16.to_le_bytes()); // valid bits
        ext_wav.extend_from_slice(&4u32.to_le_bytes()); // channel mask
        ext_wav.extend_from_slice(&1u16.to_le_bytes()); // SubFormat = PCM
        ext_wav.extend_from_slice(&KSDATAFORMAT_SUBTYPE_TAIL);
        ext_wav.extend_from_slice(b"data");
        ext_wav.extend_from_slice(&4_800u32.to_le_bytes());
        ext_wav.resize(ext_wav.len() + 4_800, 0u8);

        let e_info = inspect_wav_bytes(&ext_wav).expect("valid extensible wav");
        assert_eq!(e_info.format, WavSampleFormat::Int);
        assert_eq!(e_info.sample_rate, 24_000);
        assert_eq!(e_info.duration_ms, 100);
    }

    #[test]
    fn inspect_wav_bytes_rejects_invalid_or_truncated_inputs() {
        assert!(inspect_wav_bytes(b"short").is_err());
        assert!(inspect_wav_bytes(b"OggS0000WAVEfmt ").is_err());

        let mut truncated = build_pcm16_wav(16_000, 1, 100);
        truncated.truncate(truncated.len() - 10);
        assert!(inspect_wav_bytes(&truncated).is_err());
    }
}
