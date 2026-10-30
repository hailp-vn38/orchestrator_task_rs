//! Khởi tạo bộ System Prompts (cho 10 Agents của Flow 1 & Flow 2) và Skills dùng chung
//! trong `layout.prompts_dir()` và `layout.skills_dir()`.

use std::path::Path;

use orchestrator_flow_rs::WorkspaceLayout;
pub use orchestrator_flow_rs::{
    DAILY_ROOT_PROMPT, DAILY_SYNTHESIS_STAGING_SKILL, DAILY_TASK_REMINDER_SKILL,
    MEDIA_CREATOR_PROMPT, MEDIA_PROCESSOR_PROMPT, REMINDER_PLANNER_PROMPT, REPORT_STAGER_PROMPT,
    SOURCE_COLLECTOR_PROMPT, WIKI_CURATOR_PROMPT, WIKI_INGESTOR_PROMPT, WIKI_RESEARCHER_PROMPT,
    WIKI_ROOT_PROMPT,
};

use crate::shared::env::ExampleResult;

pub fn write_prompts_and_skills(layout: &WorkspaceLayout) -> ExampleResult<()> {
    let prompts = layout.prompts_dir();
    std::fs::create_dir_all(&prompts)?;

    // Flow 1 prompts (`daily_root` & 6 specialist subagents)
    std::fs::write(prompts.join("daily_root.md"), DAILY_ROOT_PROMPT)?;
    std::fs::write(prompts.join("source_collector.md"), SOURCE_COLLECTOR_PROMPT)?;
    std::fs::write(prompts.join("wiki_researcher.md"), WIKI_RESEARCHER_PROMPT)?;
    std::fs::write(prompts.join("report_stager.md"), REPORT_STAGER_PROMPT)?;
    std::fs::write(prompts.join("media_creator.md"), MEDIA_CREATOR_PROMPT)?;
    std::fs::write(prompts.join("media_processor.md"), MEDIA_PROCESSOR_PROMPT)?;
    std::fs::write(prompts.join("reminder_planner.md"), REMINDER_PLANNER_PROMPT)?;

    // Flow 2 prompts (`wiki_root` & 2 Wiki compilation/curation subagents)
    std::fs::write(prompts.join("wiki_root.md"), WIKI_ROOT_PROMPT)?;
    std::fs::write(prompts.join("wiki_ingestor.md"), WIKI_INGESTOR_PROMPT)?;
    std::fs::write(prompts.join("wiki_curator.md"), WIKI_CURATOR_PROMPT)?;

    // Copy 4 skills chuẩn từ `../llm-wiki-rs/skills/` sang `layout.skills_dir()` nếu có
    let skills_dst = layout.skills_dir();
    std::fs::create_dir_all(&skills_dst)?;
    let wiki_skills_src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(|p| p.join("llm-wiki-rs/skills"));
    if let Some(src_root) = wiki_skills_src.filter(|p| p.exists()) {
        for skill_name in ["wiki-taxonomy", "wiki-ingest", "wiki-query", "wiki-lint"] {
            let src_file = src_root.join(skill_name).join("SKILL.md");
            if src_file.exists() {
                let dst_dir = skills_dst.join(skill_name);
                std::fs::create_dir_all(&dst_dir)?;
                std::fs::copy(&src_file, dst_dir.join("SKILL.md"))?;
            }
        }
    }

    // Ghi 2 skills chuyên biệt của Flow 1 (`daily-synthesis-staging` & `daily-task-reminder`)
    for (skill_name, content) in [
        ("daily-synthesis-staging", DAILY_SYNTHESIS_STAGING_SKILL),
        ("daily-task-reminder", DAILY_TASK_REMINDER_SKILL),
    ] {
        let dst_dir = skills_dst.join(skill_name);
        std::fs::create_dir_all(&dst_dir)?;
        std::fs::write(dst_dir.join("SKILL.md"), content)?;
    }

    Ok(())
}
