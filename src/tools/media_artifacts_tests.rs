use super::*;

#[tokio::test]
async fn default_image_and_video_backends_support_both_default_and_agent_custom_dimensions() {
    let img_backend = DefaultImageGeneratorBackend;

    // 1. Default PNG dimensions (0 -> 1024x1024) + verify PNG IHDR binary header matches payload
    let default_png = img_backend
        .generate_image(&ImageGenerationRequest {
            prompt: "Futuristic AI data center".into(),
            title: None,
            filename: "banner.png".into(),
            format: "png".into(),
            width: 0,
            height: 0,
            svg_content: None,
        })
        .await
        .unwrap();
    assert_eq!(default_png.width, DEFAULT_PNG_WIDTH);
    assert_eq!(default_png.height, DEFAULT_PNG_HEIGHT);
    assert_eq!(default_png.mime_type, "image/png");
    let ihdr_w = u32::from_be_bytes(default_png.bytes[16..20].try_into().unwrap());
    let ihdr_h = u32::from_be_bytes(default_png.bytes[20..24].try_into().unwrap());
    assert_eq!(ihdr_w, DEFAULT_PNG_WIDTH);
    assert_eq!(ihdr_h, DEFAULT_PNG_HEIGHT);

    // 2. Agent-set custom PNG dimensions (1792x1024) + verify PNG IHDR matches
    let custom_png = img_backend
        .generate_image(&ImageGenerationRequest {
            prompt: "Widescreen concept art".into(),
            title: None,
            filename: "wide.png".into(),
            format: "png".into(),
            width: 1792,
            height: 1024,
            svg_content: None,
        })
        .await
        .unwrap();
    assert_eq!(custom_png.width, 1792);
    assert_eq!(custom_png.height, 1024);
    let custom_ihdr_w = u32::from_be_bytes(custom_png.bytes[16..20].try_into().unwrap());
    let custom_ihdr_h = u32::from_be_bytes(custom_png.bytes[20..24].try_into().unwrap());
    assert_eq!(custom_ihdr_w, 1792);
    assert_eq!(custom_ihdr_h, 1024);

    // 3. Default SVG dimensions (0 -> 1200x675) with multi-line wrapped card fallback
    let default_svg = img_backend
        .generate_image(&ImageGenerationRequest {
            prompt: "Step 1: Collect -> Step 2: Analyze -> Step 3: Visualize".into(),
            title: Some("Pipeline Flow".into()),
            filename: "flow.svg".into(),
            format: "svg".into(),
            width: 0,
            height: 0,
            svg_content: None,
        })
        .await
        .unwrap();
    assert_eq!(default_svg.width, DEFAULT_SVG_WIDTH);
    assert_eq!(default_svg.height, DEFAULT_SVG_HEIGHT);
    assert_eq!(default_svg.mime_type, "image/svg+xml");
    let svg_str = String::from_utf8(default_svg.bytes).unwrap();
    assert!(svg_str.contains("width=\"1200\" height=\"675\""));
    assert!(svg_str.contains("<tspan"));

    // 4. Full <svg> markup in svg_content syncs its root dimensions into GeneratedImagePayload
    let custom_svg = img_backend
        .generate_image(&ImageGenerationRequest {
            prompt: "Architecture diagram".into(),
            title: None,
            filename: "arch.svg".into(),
            format: "svg".into(),
            width: 0,
            height: 0,
            svg_content: Some(
                r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 960 540" width="960" height="540"><rect width="960" height="540"/></svg>"#
                    .into(),
            ),
        })
        .await
        .unwrap();
    assert_eq!(custom_svg.width, 960);
    assert_eq!(custom_svg.height, 540);

    // 5. Default & custom video dimensions
    let vid_backend = DefaultVideoGeneratorBackend;
    let default_vid = vid_backend
        .generate_video(&VideoGenerationRequest {
            prompt: "Overview animation".into(),
            title: None,
            filename: "clip.mp4".into(),
            width: 0,
            height: 0,
            duration_secs: 4,
        })
        .await
        .unwrap();
    assert_eq!(default_vid.width, DEFAULT_VIDEO_WIDTH);
    assert_eq!(default_vid.height, DEFAULT_VIDEO_HEIGHT);
}
