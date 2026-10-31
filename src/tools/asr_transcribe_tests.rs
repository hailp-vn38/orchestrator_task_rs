use super::*;
use crate::tools::asr_audio_io::KSDATAFORMAT_SUBTYPE_TAIL;

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

#[tokio::test]
async fn inspect_and_extract_wav_from_mp4_bytes_works_for_minimal_mp4() {
    let mp4 = crate::tools::media_artifacts::encode_minimal_mp4("Xin chào MP4", 640, 360, 2);
    let dur_ms = inspect_mp4_bytes(&mp4).expect("valid minimal mp4");
    assert_eq!(dur_ms, 2_000);

    let wav = extract_wav_from_mp4_bytes(&mp4)
        .await
        .expect("extracted wav from synthetic mp4");
    let header = inspect_wav_bytes(&wav).expect("valid extracted wav header");
    assert_eq!(header.sample_rate, 16_000);
    assert_eq!(header.channels, 1);
    assert_eq!(header.bits_per_sample, 16);
    assert_eq!(header.duration_ms, 2_000);

    assert!(inspect_mp4_bytes(b"not-an-mp4").is_err());
}

#[test]
fn format_segments_as_srt_renders_standard_subrip_blocks() {
    let segments = vec![
        TranscribedAudioSegment {
            start_ms: 320,
            end_ms: 3_180,
            text: "今日は新しいプロジェクトについて話します。".to_string(),
        },
        TranscribedAudioSegment {
            start_ms: 3_480,
            end_ms: 6_200,
            text: "まず最初のステップを見ていきましょう。".to_string(),
        },
    ];
    assert_eq!(format_srt_timestamp(3_661_045), "01:01:01,045");
    let srt = format_segments_as_srt(&segments);
    assert_eq!(
        srt,
        "1\n00:00:00,320 --> 00:00:03,180\n今日は新しいプロジェクトについて話します。\n\n2\n00:00:03,480 --> 00:00:06,200\nまず最初のステップを見ていきましょう。\n\n"
    );
}
