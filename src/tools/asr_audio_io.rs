//! Kiểm tra định dạng âm thanh `.wav` / container `.mp4`, bóc tách luồng âm thanh WAV từ MP4,
//! và kết xuất phụ đề chuẩn SubRip (`.srt`).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::tools::asr_transcribe::TranscribedAudioSegment;
use crate::tools::media_artifacts::{
    find_iso_bmff_box, inspect_mp4_duration_secs, synthesize_pcm_wav,
};

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

pub(crate) const KSDATAFORMAT_SUBTYPE_TAIL: [u8; 14] = [
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

/// Kiểm tra tính hợp lệ của container ISO BMFF / MP4 (`ftyp` + `moov`/`mdat`) và trả về thời lượng ước tính (`duration_ms`).
pub fn inspect_mp4_bytes(bytes: &[u8]) -> Result<u64, String> {
    if bytes.len() < 12 {
        return Err("file is shorter than 12-byte ISO BMFF / MP4 header".to_string());
    }
    if &bytes[4..8] != b"ftyp" && find_iso_bmff_box(bytes, b"ftyp").is_none() {
        return Err("missing ISO BMFF / MP4 ftyp box".to_string());
    }
    if find_iso_bmff_box(bytes, b"moov").is_none() && find_iso_bmff_box(bytes, b"mdat").is_none() {
        return Err("MP4 container is missing moov/mdat boxes".to_string());
    }
    let duration_secs = inspect_mp4_duration_secs(bytes).unwrap_or(1).max(1);
    Ok(u64::from(duration_secs).saturating_mul(1_000))
}

struct TempAsrExtractDir(PathBuf);

impl TempAsrExtractDir {
    fn create() -> Result<Self, String> {
        let dir =
            std::env::temp_dir().join(format!("orch_asr_mp4_extract_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("failed to create temp MP4 audio extract dir: {e}"))?;
        Ok(Self(dir))
    }
}

impl Drop for TempAsrExtractDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Bóc tách luồng âm thanh từ container `.mp4` thành dữ liệu `.wav` chuẩn (16 kHz, mono, 16-bit PCM).
///
/// - Với container synthetic của [`crate::tools::media_artifacts::DefaultVideoGeneratorBackend`] (`mdat` bắt đầu bằng `MP4_STREAM|`),
///   tổng hợp trực tiếp PCM WAV in-process từ thời lượng và kịch bản của clip để chạy kiểm thử độc lập không cần `ffmpeg`.
/// - Với file `.mp4` thực tế, gọi `ffmpeg -vn -ac 1 -ar 16000 -sample_fmt s16 -f wav` trong thư mục tạm có RAII guard.
pub async fn extract_wav_from_mp4_bytes(mp4_bytes: &[u8]) -> Result<Vec<u8>, String> {
    let duration_ms = inspect_mp4_bytes(mp4_bytes)?;

    if let Some(mdat) = find_iso_bmff_box(mp4_bytes, b"mdat") {
        if let Ok(stream) = std::str::from_utf8(mdat) {
            if stream.starts_with("MP4_STREAM|") {
                let prompt = stream
                    .split('|')
                    .find_map(|part| part.strip_prefix("prompt="))
                    .unwrap_or("mp4-video-audio");
                return Ok(synthesize_pcm_wav(
                    prompt,
                    16_000,
                    duration_ms.clamp(250, 10_000),
                ));
            }
        }
    }

    let temp_dir = TempAsrExtractDir::create()?;
    let input_path = temp_dir.0.join("input.mp4");
    let output_path = temp_dir.0.join("output.wav");
    std::fs::write(&input_path, mp4_bytes)
        .map_err(|e| format!("failed to write temporary MP4 file: {e}"))?;

    let output = tokio::process::Command::new("ffmpeg")
        .current_dir(&temp_dir.0)
        .args([
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-i",
            "input.mp4",
            "-vn",
            "-ac",
            "1",
            "-ar",
            "16000",
            "-sample_fmt",
            "s16",
            "-f",
            "wav",
            "output.wav",
        ])
        .output()
        .await
        .map_err(|e| format!("failed to invoke ffmpeg for MP4 audio extraction: {e}"))?;

    if !output.status.success() || !output_path.exists() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "ffmpeg could not extract audio track from MP4: {}",
            stderr.trim()
        ));
    }

    std::fs::read(&output_path).map_err(|e| format!("failed to read extracted WAV output: {e}"))
}

/// Định dạng mili-giây thành chuỗi mốc thời gian chuẩn SubRip (`HH:MM:SS,mmm`).
pub fn format_srt_timestamp(ms: u64) -> String {
    let hours = ms / 3_600_000;
    let minutes = (ms % 3_600_000) / 60_000;
    let seconds = (ms % 60_000) / 1_000;
    let millis = ms % 1_000;
    format!("{hours:02}:{minutes:02}:{seconds:02},{millis:03}")
}

/// Xuất danh sách phân đoạn [`TranscribedAudioSegment`] thành nội dung file phụ đề `.srt` chuẩn.
pub fn format_segments_as_srt(segments: &[TranscribedAudioSegment]) -> String {
    let mut out = String::new();
    let mut idx = 1usize;
    for seg in segments {
        let text = seg.text.trim();
        if text.is_empty() {
            continue;
        }
        let start = format_srt_timestamp(seg.start_ms);
        let end = format_srt_timestamp(seg.end_ms.max(seg.start_ms + 1));
        out.push_str(&format!("{idx}\n{start} --> {end}\n{text}\n\n"));
        idx += 1;
    }
    out
}
