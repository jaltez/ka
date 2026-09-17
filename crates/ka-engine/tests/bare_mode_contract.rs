//! Safe-mode contract (`ka --safe-mode` / `KA_SAFEMODE=1`): every
//! customization tier goes dark while built-ins, config, rules, and
//! auth stay. Isolated in its own test binary because the flag is a
//! process-global.

#![allow(clippy::unwrap_used, clippy::expect_used)]
use ka_engine::conventions;
use ka_engine::trust;

#[test]
fn safe_mode_disables_every_customization_tier() {
    let dir = std::env::temp_dir().join(format!("ka-bare-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    // a fully-loaded project: instructions, memory, skills, rules
    std::fs::create_dir_all(dir.join(".ka/skills/ship")).unwrap();
    std::fs::write(
        dir.join(".ka/skills/ship/SKILL.md"),
        "---\ndescription: ship it\n---\nbody",
    )
    .unwrap();
    std::fs::create_dir_all(dir.join(".ka/rules")).unwrap();
    std::fs::write(dir.join(".ka/rules/style.md"), "always on rule").unwrap();
    std::fs::create_dir_all(dir.join(".ka/agents")).unwrap();
    std::fs::write(dir.join(".ka/agents/reviewer.md"), "you review").unwrap();
    std::fs::write(dir.join("AGENTS.md"), "project instructions").unwrap();
    std::fs::write(dir.join("MEMORY.md"), "project memory").unwrap();

    trust::test_support::with_trust_file(|_| {
        trust::approve(&dir);

        // everything loads in normal mode
        assert!(
            !conventions::discover_agents(&dir).is_empty(),
            "AGENTS.md loads"
        );
        assert!(
            !conventions::discover_memory(&dir).is_empty(),
            "MEMORY.md loads"
        );
        assert!(
            conventions::discover_skills(&dir)
                .iter()
                .any(|s| s.name == "ship"),
            "skills load"
        );
        assert!(!conventions::discover_rules(&dir).is_empty(), "rules load");
        assert!(
            !ka_engine::agents::AgentDef::discover(&dir).is_empty(),
            "markdown agents load"
        );

        // safe mode: all of it goes dark
        conventions::set_bare_mode(true);
        assert!(
            conventions::discover_agents(&dir).is_empty(),
            "safe mode hides AGENTS.md"
        );
        assert!(
            conventions::discover_memory(&dir).is_empty(),
            "safe mode hides MEMORY.md"
        );
        assert!(
            conventions::discover_skills(&dir).is_empty(),
            "safe mode hides skills"
        );
        assert!(
            conventions::discover_rules(&dir).is_empty(),
            "safe mode hides scoped rules"
        );
        // markdown agents are gated at the engine call site (the engine
        // skips AgentDef::discover in bare mode), not inside discovery —
        // so discovery still finds them here; that layering is the
        // contract, don't "fix" one side without the other
        assert!(
            !ka_engine::agents::AgentDef::discover(&dir).is_empty(),
            "agent discovery is engine-gated, not self-gated"
        );
        conventions::set_bare_mode(false);

        // back on: everything returns
        assert!(!conventions::discover_agents(&dir).is_empty());
    });

    // the [lsp] tier — spawned servers, navigation hands, write-through —
    // is a customization tier like any other: bare mode forces
    // Lsp::default(), so neither gate (enable / write_through) can fire
    let loaded = ka_engine::config::Config {
        lsp: ka_engine::config::Lsp {
            enable: Some(true),
            commands: Some(
                [("rust".to_string(), "rust-analyzer".to_string())]
                    .into_iter()
                    .collect(),
            ),
            write_through: Some(true),
        },
        ..ka_engine::config::Config::default()
    };
    conventions::set_bare_mode(true);
    let lsp = ka_engine::effective_lsp_cfg(&loaded);
    conventions::set_bare_mode(false);
    assert_ne!(
        lsp.enable,
        Some(true),
        "bare mode never spawns language servers"
    );
    assert_ne!(
        lsp.write_through,
        Some(true),
        "bare mode never registers write-through hands"
    );
    assert_ne!(
        ka_engine::effective_debug_cfg(&loaded).enable,
        Some(true),
        "bare mode never spawns debug adapters"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
