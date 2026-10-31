# File code dài hơn 500 dòng

Phạm vi: file `.rs`, `.sh`, `.py` trong project; tuân theo ignore của ripgrep/Git.
Đếm toàn bộ dòng, gồm dòng trống, comment và dòng cuối chưa có newline.

Tạo lại bằng: `python3 scripts/list-long-code-files.py --threshold 500`.

Tổng cộng: **4 file** (sắp xếp số dòng giảm dần).

| Path | Số dòng |
| --- | ---: |
| `src/tools/media_video.rs` | 548 |
| `src/tools/media_video_concat.rs` | 547 |
| `src/tools/youtube.rs` | 544 |
| `src/tools/media_image_codecs.rs` | 543 |
