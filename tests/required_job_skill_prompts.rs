use orchestrator_flow_rs::{WIKI_CURATOR_PROMPT, WIKI_INGESTOR_PROMPT};

#[test]
fn wiki_ingestor_export_uses_preloaded_workflow_and_parallel_first_turn_reads() {
    for required_skill in ["wiki-ingest", "wiki-taxonomy", "wiki-page-format"] {
        assert!(
            !WIKI_INGESTOR_PROMPT.contains(&format!("skill(id=\"{required_skill}\")")),
            "flow must export the prompt without a load call for {required_skill}"
        );
    }
    assert!(WIKI_INGESTOR_PROMPT.contains("already loaded below this prompt"));
    assert!(WIKI_INGESTOR_PROMPT.contains("wiki_get(file=\"wiki/index.md\")"));
    assert!(WIKI_INGESTOR_PROMPT.contains("wiki_get(file=\"sources/<source_rel_path>\")"));
    assert!(WIKI_INGESTOR_PROMPT.contains("together in one turn"));
}

#[test]
fn wiki_curator_export_uses_preloaded_workflow() {
    for required_skill in ["wiki-lint", "wiki-query"] {
        assert!(
            !WIKI_CURATOR_PROMPT.contains(&format!("skill` with `id=\"{required_skill}\"`")),
            "flow must export the prompt without a load call for {required_skill}"
        );
    }
    assert!(WIKI_CURATOR_PROMPT.contains("already loaded below this prompt"));
    assert!(WIKI_CURATOR_PROMPT.contains("wiki_get(file=\"wiki/index.md\")"));
}
