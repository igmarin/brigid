#![allow(missing_docs)]

use minijinja::{Environment, context};

fn render(template: &str, ctx: minijinja::Value) -> String {
    let mut env = Environment::new();
    env.add_template("t", template)
        .expect("failed to add template");
    env.get_template("t")
        .expect("failed to get template")
        .render(ctx)
        .expect("failed to render template")
}

macro_rules! render_prompt {
    ($name:literal, $ctx:expr) => {
        render(
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/prompts/",
                $name
            )),
            $ctx,
        )
    };
}

#[test]
fn identify_single_shot_renders_with_expected_markers() {
    let out = render_prompt!(
        "identify_single_shot.md.j2",
        context! {
            project_name => "brigid",
            context => "File snippets here",
            language_instruction => "",
            max_abstraction_num => 10,
            name_lang_hint => "",
            desc_lang_hint => "",
            file_listing => "- 0 # lib.rs\n- 1 # main.rs",
        }
    );
    assert!(out.contains("```yaml"), "missing yaml fence");
    assert!(out.contains("name:"), "missing name field");
    assert!(out.contains("description:"), "missing description field");
    assert!(out.contains("file_indices:"), "missing file_indices field");
    assert!(out.contains("brigid"), "missing project name");
    assert!(out.contains("top 5-10"), "missing max abstractions range");
}

#[test]
fn identify_map_renders_with_expected_markers() {
    let out = render_prompt!(
        "identify_map.md.j2",
        context! {
            batch_idx => 1,
            batch_total => 3,
            project_name => "brigid",
            module_note => "core",
            context => "File snippets for this batch",
            language_instruction => "",
            per_batch => 5,
            name_lang_hint => "",
            desc_lang_hint => "",
            file_listing => "- 0 # lib.rs",
        }
    );
    assert!(out.contains("batch 1/3"), "missing batch indicator");
    assert!(out.contains("up to 5 important"), "missing per_batch count");
    assert!(out.contains("```yaml"), "missing yaml fence");
    assert!(out.contains("name:"), "missing name field");
    assert!(out.contains("description:"), "missing description field");
    assert!(out.contains("file_indices:"), "missing file_indices field");
}

#[test]
fn identify_reduce_renders_with_expected_markers() {
    let out = render_prompt!(
        "identify_reduce.md.j2",
        context! {
            project_name => "brigid",
            module_summary => "core, cli",
            language_instruction => "",
            max_abstraction_num => 10,
            name_lang_hint => "",
            desc_lang_hint => "",
            candidates_blob => "- candidate 0:\n    name: Query Processing\n    description: Handles queries\n    file_indices: [0, 1]",
        }
    );
    assert!(out.contains("top 5-10"), "missing max abstractions range");
    assert!(out.contains("(max 10 items):"), "missing max item count");
    assert!(out.contains("```yaml"), "missing yaml fence");
    assert!(
        out.contains("Final Name"),
        "missing final abstraction example"
    );
    assert!(out.contains("file_indices:"), "missing file_indices field");
}

#[test]
fn analyze_relationships_renders_with_expected_markers() {
    let out = render_prompt!(
        "analyze_relationships.md.j2",
        context! {
            project_name => "brigid",
            list_lang_note => "",
            abstraction_listing => "- 0 # Query Processing\n- 1 # Optimization",
            context => "Abstractions and code snippets",
            language_instruction => "",
            monorepo_instruction => "",
            lang_hint => "",
        }
    );
    assert!(out.contains("```yaml"), "missing yaml fence");
    assert!(out.contains("summary:"), "missing summary field");
    assert!(
        out.contains("relationships:"),
        "missing relationships field"
    );
    assert!(
        out.contains("from_abstraction:"),
        "missing from_abstraction field"
    );
    assert!(
        out.contains("to_abstraction:"),
        "missing to_abstraction field"
    );
    assert!(out.contains("label:"), "missing label field");
    assert!(out.contains("kind:"), "missing kind field");
    assert!(out.contains("Manages"), "missing example label");
    assert!(out.contains("Provides config"), "missing example label");
}

#[test]
fn order_chapters_renders_with_expected_markers() {
    let out = render_prompt!(
        "order_chapters.md.j2",
        context! {
            project_name => "brigid",
            list_lang_note => "",
            abstraction_listing => "- 0 # Query Processing\n- 1 # Optimization",
            context => "Project summary and relationships",
        }
    );
    assert!(out.contains("```yaml"), "missing yaml fence");
    assert!(
        out.contains("# AbstractionName"),
        "missing ordering example"
    );
}

#[test]
fn chapter_outline_renders_with_expected_markers() {
    let out = render_prompt!(
        "chapter_outline.md.j2",
        context! {
            lang => "English",
            tier => "M",
            diagram_level => "standard",
            need => 2,
        }
    );
    assert!(
        out.contains("## MANDATORY CHAPTER STRUCTURE"),
        "missing structure section"
    );
    assert!(
        out.contains("## DIAGRAM REQUIREMENTS"),
        "missing diagram section"
    );
    assert!(
        out.contains("## GROUNDING RULES"),
        "missing grounding section"
    );
    assert!(out.contains("mermaid"), "missing mermaid mention");
    assert!(out.contains("**M**"), "missing tier value");
    assert!(out.contains("**standard**"), "missing diagram level");
    assert!(out.contains("2"), "missing diagram count");
}

#[test]
fn write_chapter_renders_with_expected_markers() {
    let outline = render_prompt!(
        "chapter_outline.md.j2",
        context! {
            lang => "English",
            tier => "M",
            diagram_level => "standard",
            need => 2,
        }
    );

    let out = render_prompt!(
        "write_chapter.md.j2",
        context! {
            language_instruction => "",
            project_name => "brigid",
            abstraction_name => "Query Processing",
            chapter_num => 1,
            abstraction_description => "Handles incoming queries",
            tier => "M",
            kind => "service",
            apps_line => "core",
            entry_list => "- `lib.rs`",
            full_chapter_listing => "- [Query Processing](01_query_processing.md)",
            prev_link => "None (first chapter)",
            next_link => "None (last chapter)",
            previous_chapters_summary => "This is the first chapter.",
            file_context_str => "--- File: lib.rs ---\nfn main() {}",
            chapter_outline => outline,
            need => 2,
        }
    );
    assert!(
        out.contains("Write a beginner-friendly tutorial chapter"),
        "missing chapter instruction"
    );
    assert!(out.contains("Query Processing"), "missing abstraction name");
    assert!(
        out.contains("This is Chapter 1.\n"),
        "missing chapter number"
    );
    assert!(
        out.contains("Tier: M | Kind: service | Apps: core"),
        "missing concept metadata"
    );
    assert!(out.contains("## Motivation"), "missing Motivation section");
    assert!(out.contains("## Core idea"), "missing Core idea section");
    assert!(
        out.contains("## Mental model"),
        "missing Mental model section"
    );
    assert!(
        out.contains("## How to use it"),
        "missing How to use it section"
    );
    assert!(
        out.contains("## Under the hood"),
        "missing Under the hood section"
    );
    assert!(out.contains("## Key files"), "missing Key files section");
    assert!(
        out.contains("## Connections"),
        "missing Connections section"
    );
    assert!(out.contains("## Pitfalls"), "missing Pitfalls section");
    assert!(out.contains("## Summary"), "missing Summary section");
    assert!(
        out.contains("Minimum mermaid diagrams required: 2"),
        "missing diagram requirement"
    );
}

#[test]
fn review_chapter_renders_with_expected_markers() {
    let out = render_prompt!(
        "review_chapter.md.j2",
        context! {
            language => "English",
            need => 2,
            have => 1,
            chapter_md => "# Chapter 1: Query Processing\n\ncontent",
        }
    );
    assert!(
        out.contains("Review and lightly improve"),
        "missing review instruction"
    );
    assert!(out.contains("Language: English"), "missing language line");
    assert!(
        out.contains("Ensure at least 2 mermaid diagrams"),
        "missing diagram requirement"
    );
    assert!(
        out.contains("(currently 1)."),
        "missing current diagram count"
    );
    assert!(out.contains("Chapter:"), "missing chapter marker");
    assert!(out.contains("Output ONLY"), "missing output instruction");
}

#[test]
fn write_setup_guide_renders_with_expected_markers() {
    let out = render_prompt!(
        "write_setup_guide.md.j2",
        context! {
            project_name => "brigid",
            score => 50,
            gaps => "- Missing env setup",
            context => "README fragment and config files",
            lang => "English",
        }
    );
    assert!(out.contains("# Setup: brigid"), "missing setup heading");
    assert!(
        out.contains("## Prerequisites"),
        "missing Prerequisites section"
    );
    assert!(
        out.contains("## Install dependencies"),
        "missing Install dependencies section"
    );
    assert!(
        out.contains("## Environment configuration"),
        "missing Environment configuration section"
    );
    assert!(
        out.contains("## Run locally"),
        "missing Run locally section"
    );
    assert!(
        out.contains("## Verify the install"),
        "missing Verify the install section"
    );
    assert!(out.contains("score 50/100"), "missing docs score line");
}

#[test]
fn write_architecture_overview_renders_with_expected_markers() {
    let out = render_prompt!(
        "write_architecture_overview.md.j2",
        context! {
            lang_note => "",
            project_name => "brigid",
            summary => "A Rust tutorial generator",
            inventory => "- core: 5 files",
            abstractions => "- 0: Query Processing",
            relationships => "- 1 -> 0: uses",
        }
    );
    assert!(
        out.contains("# Architecture overview"),
        "missing architecture heading"
    );
    assert!(
        out.contains("## What kind of system this is"),
        "missing system kind section"
    );
    assert!(
        out.contains("## How the monorepo is carved up"),
        "missing carve-up section"
    );
    assert!(
        out.contains("## How apps collaborate"),
        "missing collaboration section"
    );
    assert!(
        out.contains("## Suggested reading order"),
        "missing reading order section"
    );
    assert!(
        out.contains("## Mental model diagram"),
        "missing mental model section"
    );
    assert!(out.contains("mermaid"), "missing mermaid mention");
}

macro_rules! test_prompt_minimal {
    ($fn_name:ident, $file:literal, $ctx:expr) => {
        #[test]
        fn $fn_name() {
            let out = render_prompt!($file, $ctx);
            let file_name: &str = $file;
            assert!(
                !out.contains("{{"),
                "{file_name} still contains unrendered placeholders"
            );
        }
    };
}

test_prompt_minimal!(
    identify_single_shot_minimal,
    "identify_single_shot.md.j2",
    context! {
        project_name => "x",
        context => "",
        language_instruction => "",
        max_abstraction_num => 5,
        name_lang_hint => "",
        desc_lang_hint => "",
        file_listing => "",
    }
);

test_prompt_minimal!(
    identify_map_minimal,
    "identify_map.md.j2",
    context! {
        batch_idx => 1,
        batch_total => 1,
        project_name => "x",
        module_note => "",
        context => "",
        language_instruction => "",
        per_batch => 1,
        name_lang_hint => "",
        desc_lang_hint => "",
        file_listing => "",
    }
);

test_prompt_minimal!(
    identify_reduce_minimal,
    "identify_reduce.md.j2",
    context! {
        project_name => "x",
        module_summary => "",
        language_instruction => "",
        max_abstraction_num => 5,
        name_lang_hint => "",
        desc_lang_hint => "",
        candidates_blob => "",
    }
);

test_prompt_minimal!(
    analyze_relationships_minimal,
    "analyze_relationships.md.j2",
    context! {
        project_name => "x",
        list_lang_note => "",
        abstraction_listing => "",
        context => "",
        language_instruction => "",
        monorepo_instruction => "",
        lang_hint => "",
    }
);

test_prompt_minimal!(
    order_chapters_minimal,
    "order_chapters.md.j2",
    context! {
        project_name => "x",
        list_lang_note => "",
        abstraction_listing => "",
        context => "",
    }
);

test_prompt_minimal!(
    chapter_outline_minimal,
    "chapter_outline.md.j2",
    context! {
        lang => "English",
        tier => "S",
        diagram_level => "minimal",
        need => 0,
    }
);

test_prompt_minimal!(
    write_chapter_minimal,
    "write_chapter.md.j2",
    context! {
        language_instruction => "",
        project_name => "x",
        abstraction_name => "A",
        chapter_num => 1,
        abstraction_description => "",
        tier => "S",
        kind => "",
        apps_line => "",
        entry_list => "",
        full_chapter_listing => "",
        prev_link => "",
        next_link => "",
        previous_chapters_summary => "",
        file_context_str => "",
        chapter_outline => "## MANDATORY CHAPTER STRUCTURE\n## DIAGRAM REQUIREMENTS\n## GROUNDING RULES",
        need => 0,
    }
);

test_prompt_minimal!(
    review_chapter_minimal,
    "review_chapter.md.j2",
    context! {
        language => "English",
        need => 0,
        have => 0,
        chapter_md => "",
    }
);

test_prompt_minimal!(
    write_setup_guide_minimal,
    "write_setup_guide.md.j2",
    context! {
        project_name => "x",
        score => 0,
        gaps => "",
        context => "",
        lang => "English",
    }
);

test_prompt_minimal!(
    write_architecture_overview_minimal,
    "write_architecture_overview.md.j2",
    context! {
        lang_note => "",
        project_name => "x",
        summary => "",
        inventory => "",
        abstractions => "",
        relationships => "",
    }
);
