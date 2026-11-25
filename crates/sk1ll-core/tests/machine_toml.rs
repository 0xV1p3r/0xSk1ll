//! `machine.toml`: what users write (every error they can hit), what
//! presets detection makes of it, and edits that keep their file.

use sk1ll_core::{Error, Harness, HarnessChange, HarnessOrigin, InstructionsFormat, LinkMode, MachineConfig, Result};
use sk1ll_core::{config_file::Snapshot, machine::MACHINE_FILE};
use sk1ll_testkit::{Home, child};
use std::{
    fs,
    path::{Path, PathBuf},
};

#[test]
fn notification_preference_defaults_on_and_preserves_other_config() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("machine.toml");
    assert!(MachineConfig::load(&path).unwrap().notifications.enabled);
    std::fs::write(&path, "# keep this\nid = 'box'\n").unwrap();
    MachineConfig::set_notifications(&path, false).unwrap();
    assert!(!MachineConfig::load(&path).unwrap().notifications.enabled);
    let written = std::fs::read_to_string(&path).unwrap();
    assert!(written.starts_with("# keep this\nid = 'box'"));
    MachineConfig::set_notifications(&path, true).unwrap();
    assert!(MachineConfig::load(&path).unwrap().notifications.enabled);
}

fn config(toml: &str) -> MachineConfig {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(MACHINE_FILE);
    fs::write(&path, toml).unwrap();
    MachineConfig::load(&path).unwrap()
}

#[test]
fn detects_presets_by_home_dir() {
    let Some(_home) = isolated_home("detects_presets_by_home_dir", &[".claude", ".config/opencode"]) else { return };
    let eff = MachineConfig::default().effective();
    assert_eq!(eff.keys().collect::<Vec<_>>(), ["claude", "opencode"]);
    assert_eq!(eff["claude"].origin, HarnessOrigin::Detected);
    assert_eq!(eff["claude"].harness.instructions_format, InstructionsFormat::Rules);
    assert_eq!(eff["claude"].harness.skills.as_deref(), Some("~/.claude/skills"));
}

#[test]
fn entries_override_extend_and_disable() {
    let Some(_home) = isolated_home("entries_override_extend_and_disable", &[".claude", ".codex", ".gemini"]) else {
        return;
    };
    let cfg = config(
        r#"
        id = "workstation"
        [harnesses.claude]
        mode = "copy"
        instructions = ""
        [harnesses.codex]
        enabled = false
        [harnesses.agents]
        skills = "/opt/agents/skills"
        "#,
    );
    assert_eq!(cfg.id, "workstation");
    let eff = cfg.effective();
    assert_eq!(eff.keys().collect::<Vec<_>>(), ["agents", "claude", "gemini"]);
    let claude = &eff["claude"];
    assert_eq!(claude.origin, HarnessOrigin::Override);
    assert_eq!(claude.harness.mode, LinkMode::Copy);
    assert_eq!(claude.harness.skills.as_deref(), Some("~/.claude/skills"));
    assert_eq!(claude.harness.instructions, None);
    assert_eq!(eff["agents"].origin, HarnessOrigin::Local);
    assert_eq!(eff["agents"].harness.skills.as_deref(), Some("/opt/agents/skills"));
    assert_eq!(eff["gemini"].origin, HarnessOrigin::Detected);
}

#[test]
fn choices_list_disabled_presets_and_set_enabled_keeps_comments() {
    let Some(_home) =
        isolated_home("choices_list_disabled_presets_and_set_enabled_keeps_comments", &[".claude", ".codex"])
    else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sk1ll").join(MACHINE_FILE);
    MachineConfig::set_enabled(&path, "codex", false).unwrap();
    fs::write(&path, format!("# mine\nid = \"box\"\n{}", fs::read_to_string(&path).unwrap())).unwrap();
    let choices = MachineConfig::load(&path).unwrap().choices();
    let get = |name: &str| choices.iter().find(|c| c.name == name).unwrap().clone();
    assert!(get("claude").installed && get("claude").enabled);
    assert!(get("codex").installed && !get("codex").enabled);
    assert!(!get("gemini").installed && !get("gemini").enabled);
    assert_eq!(get("codex").harness.skills.as_deref(), Some("~/.codex/skills"));

    MachineConfig::set_enabled(&path, "codex", true).unwrap();
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.starts_with("# mine\nid = \"box\"\n"), "{text}");
    assert!(!text.contains("codex"), "{text}");
    assert!(MachineConfig::load(&path).unwrap().effective().contains_key("codex"));
    assert!(!path.with_extension("toml.tmp").exists());

    fs::write(&path, "[harnesses.agents]\nskills = \"/opt/s\"\n").unwrap();
    MachineConfig::set_enabled(&path, "agents", false).unwrap();
    MachineConfig::set_enabled(&path, "agents", true).unwrap();
    let cfg = MachineConfig::load(&path).unwrap();
    assert_eq!(cfg.harnesses["agents"].skills.as_deref(), Some("/opt/s"));
    assert_eq!(cfg.harnesses["agents"].enabled, None);
}

#[test]
fn harness_changes_keep_the_file_and_write_only_what_differs() {
    let Some(_home) = isolated_home("harness_changes_keep_the_file_and_write_only_what_differs", &[".codex"]) else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(MACHINE_FILE);
    fs::write(&path, "# mine\nid = \"box\" # stays\n").unwrap();
    let edit = |change: HarnessChange| edit(&path, change);
    let set = |name: &str, harness: Harness| HarnessChange::Set { name: name.into(), harness };

    // A custom harness writes its paths and non-default keys.
    let custom = Harness {
        skills: Some("~/.agents/skills".into()),
        instructions: Some("/opt/agents/rules".into()),
        instructions_format: InstructionsFormat::Rules,
        mode: LinkMode::Symlink,
    };
    let (text, cfg) = edit(set("agents", custom.clone())).unwrap();
    assert!(text.starts_with("# mine\nid = \"box\" # stays\n"), "{text}");
    assert!(!text.contains("mode"), "{text}");
    assert_eq!(cfg.choices().into_iter().find(|c| c.name == "agents").unwrap().harness, custom);

    // A preset gets only its differences; the same settings drop the entry.
    let mut codex = preset_harness("codex");
    codex.instructions = None;
    let (text, _) = edit(set("codex", codex)).unwrap();
    assert!(text.contains("[harnesses.codex]\ninstructions = \"\"\n"), "{text}");
    let (text, _) = edit(set("codex", preset_harness("codex"))).unwrap();
    assert!(!text.contains("codex"), "{text}");

    // Enabling a preset that isn't installed includes it explicitly.
    let (_, cfg) = edit(HarnessChange::Enable { name: "gemini".into() }).unwrap();
    assert!(cfg.effective().contains_key("gemini"));
    let (_, cfg) = edit(HarnessChange::Disable { name: "gemini".into() }).unwrap();
    assert!(!cfg.effective().contains_key("gemini"));
    let (_, cfg) = edit(HarnessChange::Remove { name: "agents".into() }).unwrap();
    assert!(!cfg.harnesses.contains_key("agents"));
}

/// Makes `change` to the `machine.toml` at `path` with `home` as home.
fn edit(path: &Path, change: HarnessChange) -> Result<(String, MachineConfig)> {
    let snapshot = Snapshot::take(path)?;
    let edit = MachineConfig::edit(&snapshot, &change)?;
    edit.write(&snapshot)?;
    Ok((edit.text, edit.config))
}

/// Preset detection reads the process HOME. Give each case its own in a child.
fn isolated_home(test: &str, dirs: &[&str]) -> Option<PathBuf> {
    if let Some(home) = child::dir() {
        for dir in dirs {
            fs::create_dir_all(home.join(dir)).unwrap();
        }
        return Some(home);
    }
    let home = Home::new();
    let out = home.apply(&mut child::command(test, home.path())).output().unwrap();
    let result = sk1ll_testkit::text(&out);
    assert!(out.status.success() && result.contains("1 passed"), "{result}");
    None
}

#[test]
fn opt_in_presets_stay_off_on_an_upgraded_machine() {
    let Some(_home) =
        isolated_home("opt_in_presets_stay_off_on_an_upgraded_machine", &[".claude", ".gemini", ".kiro", ".cursor"])
    else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(MACHINE_FILE);
    // A 0.1.0 machine.toml.
    fs::write(&path, "id = \"box\"\n[harnesses.codex]\nenabled = false\n").unwrap();
    let cfg = MachineConfig::load(&path).unwrap();
    let eff = cfg.effective();
    assert_eq!(eff.keys().collect::<Vec<_>>(), ["claude", "gemini"]);
    assert_eq!(eff["gemini"].harness.skills, None, "gemini syncs only its instructions, as before");
    let choices = cfg.choices();
    let get = |name: &str| choices.iter().find(|c| c.name == name).unwrap().clone();
    assert!(get("kiro").installed && !get("kiro").enabled && get("kiro").available);
    assert_eq!(get("kiro").harness.skills.as_deref(), Some("~/.kiro/skills"), "what enabling it syncs into");
    assert!(get("gemini").enabled && get("gemini").available);
    assert!(get("claude").enabled && !get("claude").available);
    assert!(!get("copilot").installed && !get("copilot").enabled);

    // Opting in is written explicitly, and stays so.
    edit(&path, HarnessChange::Enable { name: "kiro".into() }).unwrap();
    edit(&path, HarnessChange::Enable { name: "gemini".into() }).unwrap();
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains("[harnesses.kiro]\nenabled = true\n"), "{text}");
    assert!(text.contains("[harnesses.gemini]\nskills = \"~/.gemini/skills\"\n"), "{text}");
    let eff = MachineConfig::load(&path).unwrap().effective();
    assert_eq!(eff.keys().collect::<Vec<_>>(), ["claude", "gemini", "kiro"]);
    assert_eq!(eff["gemini"].harness.skills.as_deref(), Some("~/.gemini/skills"));
    assert_eq!(eff["kiro"].harness.instructions.as_deref(), Some("~/.kiro/steering"));
    // Off again, and on: the entry says so each time.
    edit(&path, HarnessChange::Disable { name: "kiro".into() }).unwrap();
    assert!(!MachineConfig::load(&path).unwrap().effective().contains_key("kiro"));

    // Without machine.toml at all, the same.
    let bare = tempfile::tempdir().unwrap();
    let eff = MachineConfig::load(&bare.path().join(MACHINE_FILE)).unwrap().effective();
    assert_eq!(eff.keys().collect::<Vec<_>>(), ["claude", "gemini"]);
}

#[test]
fn first_run_setup_enables_the_opt_in_presets_it_finds() {
    let Some(_home) = isolated_home(
        "first_run_setup_enables_the_opt_in_presets_it_finds",
        &[".claude", ".gemini", ".kiro", ".cursor"],
    ) else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(MACHINE_FILE);
    // Setup turned cursor off; the listing shows the rest as setup leaves them, writing nothing.
    MachineConfig::set_enabled(&path, "cursor", false).unwrap();
    let before = fs::read_to_string(&path).unwrap();
    let repo = dir.path().join("repo");
    let listed = MachineConfig::setup_choices(&path, &repo).unwrap();
    let on: Vec<_> = listed.iter().filter(|c| c.enabled).map(|c| c.name.as_str()).collect();
    assert_eq!(on, ["claude", "gemini", "kiro"]);
    assert_eq!(fs::read_to_string(&path).unwrap(), before, "listing writes nothing");

    assert!(MachineConfig::adopt_detected_presets(&path, &repo).unwrap().is_empty());
    let text = fs::read_to_string(&path).unwrap();
    let eff = MachineConfig::load(&path).unwrap().effective();
    assert_eq!(eff.keys().collect::<Vec<_>>(), ["claude", "gemini", "kiro"], "{text}");
    assert_eq!(eff["gemini"].harness.skills.as_deref(), Some("~/.gemini/skills"));
    assert!(text.contains("[harnesses.cursor]\nenabled = false\n"), "{text}");
    // Taken up once: running it again changes nothing.
    MachineConfig::adopt_detected_presets(&path, &repo).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), text);
}

#[test]
fn setup_skips_adoption_when_destinations_overlap_harnesses_or_repo() {
    let Some(_home) = isolated_home(
        "setup_skips_adoption_when_destinations_overlap_harnesses_or_repo",
        &[".kiro", ".cursor", ".roo"],
    ) else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(MACHINE_FILE);
    fs::write(&path, "[harnesses.local]\nskills = '~/.kiro/skills/nested'\n").unwrap();
    let repo = _home.join(".cursor/skills/repo");
    let choices = MachineConfig::setup_choices(&path, &repo).unwrap();
    let warning = |name| choices.iter().find(|choice| choice.name == name).unwrap().setup_warning.as_deref();
    assert_eq!(warning("kiro"), Some("not enabled: overlaps local"));
    assert_eq!(warning("cursor"), Some("not enabled: overlaps the skills repo"));
    assert_eq!(
        MachineConfig::adopt_detected_presets(&path, &repo).unwrap(),
        ["cursor: not enabled: overlaps the skills repo", "kiro: not enabled: overlaps local"]
    );
    let effective = MachineConfig::load(&path).unwrap().effective();
    assert_eq!(effective.keys().map(String::as_str).collect::<Vec<_>>(), ["local", "roo"]);
}

#[test]
fn a_custom_harness_named_like_an_opt_in_preset_keeps_its_paths() {
    let Some(_home) = isolated_home("a_custom_harness_named_like_an_opt_in_preset_keeps_its_paths", &[".kiro"]) else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(MACHINE_FILE);
    fs::write(&path, "[harnesses.kiro]\nenabled = true\nskills = \"/my/skills\"\n").unwrap();
    let own = Harness { skills: Some("/my/skills".into()), ..Harness::default() };
    let cfg = MachineConfig::load(&path).unwrap();
    let eff = cfg.effective();
    assert_eq!(eff["kiro"].origin, HarnessOrigin::Local);
    assert_eq!(eff["kiro"].harness, own);
    let choices = cfg.choices();
    assert_eq!(choices.iter().filter(|c| c.name == "kiro").count(), 1);
    assert!(choices.iter().any(|c| c.name == "kiro" && !c.preset && c.enabled));
    // First-run setup leaves it alone.
    let before = fs::read(&path).unwrap();
    MachineConfig::adopt_detected_presets(&path, &dir.path().join("repo")).unwrap();
    assert_eq!(fs::read(&path).unwrap(), before);

    // Off and on again: still exactly its own paths.
    edit(&path, HarnessChange::Disable { name: "kiro".into() }).unwrap();
    let (_, cfg) = edit(&path, HarnessChange::Enable { name: "kiro".into() }).unwrap();
    assert_eq!(cfg.effective()["kiro"].harness, own);
    // Editing it keeps it custom: nothing of the preset comes in.
    let (_, cfg) = edit(
        &path,
        HarnessChange::Set {
            name: "kiro".into(),
            harness: Harness { skills: Some("/opt/kiro2".into()), mode: LinkMode::Copy, ..Harness::default() },
        },
    )
    .unwrap();
    assert_eq!(cfg.effective()["kiro"].harness.instructions, None);
    // Removing it leaves the preset available, still off.
    let (_, cfg) = edit(&path, HarnessChange::Remove { name: "kiro".into() }).unwrap();
    assert!(!cfg.effective().contains_key("kiro"));
    assert!(cfg.choices().iter().any(|c| c.name == "kiro" && c.preset && c.available));
}

#[test]
fn an_opt_in_preset_with_paths_of_its_own_reads_the_same_when_off() {
    let Some(_home) = isolated_home("an_opt_in_preset_with_paths_of_its_own_reads_the_same_when_off", &[".kiro"])
    else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(MACHINE_FILE);
    let mut kiro = preset_harness("kiro");
    kiro.skills = Some("/opt/kiro-skills".into());
    // Setting it takes it up, written out in full.
    let (_, cfg) = edit(&path, HarnessChange::Set { name: "kiro".into(), harness: kiro.clone() }).unwrap();
    assert_eq!(cfg.effective()["kiro"].harness, kiro);
    edit(&path, HarnessChange::Disable { name: "kiro".into() }).unwrap();
    let (_, cfg) = edit(&path, HarnessChange::Enable { name: "kiro".into() }).unwrap();
    assert_eq!(cfg.effective()["kiro"].harness, kiro);
}

#[test]
fn setting_never_switches_off_a_harness_that_was_on() {
    // Not installed here, so on only through its entry.
    let Some(_home) = isolated_home("setting_never_switches_off_a_harness_that_was_on", &[]) else { return };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(MACHINE_FILE);
    fs::write(&path, "[harnesses.codex]\nmode = \"symlink\"\n[harnesses.kiro]\nenabled = true\nmode = \"symlink\"\n")
        .unwrap();
    for name in ["codex", "kiro"] {
        let set = HarnessChange::Set { name: name.into(), harness: preset_harness(name) };
        let (text, cfg) = edit(&path, set).unwrap();
        assert!(cfg.effective().contains_key(name), "{name}: {text}");
        assert!(text.contains(&format!("[harnesses.{name}]\nenabled = true\n")), "{name}: {text}");
    }
}

#[test]
fn setting_a_preset_value_again_drops_the_override() {
    let Some(_home) = isolated_home("setting_a_preset_value_again_drops_the_override", &[".claude"]) else { return };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(MACHINE_FILE);
    let set = |harness: Harness| HarnessChange::Set { name: "claude".into(), harness };
    let mut claude = preset_harness("claude");
    claude.mode = LinkMode::Copy;
    claude.instructions_format = InstructionsFormat::Block;
    claude.instructions = Some("~/.claude/CLAUDE.md".into());
    let (text, _) = edit(&path, set(claude.clone())).unwrap();
    assert!(text.contains("mode = \"copy\"") && text.contains("instructions_format = \"block\""), "{text}");
    claude.mode = LinkMode::Symlink;
    let (text, _) = edit(&path, set(claude.clone())).unwrap();
    assert!(!text.contains("mode"), "{text}");
    let (text, cfg) = edit(&path, set(preset_harness("claude"))).unwrap();
    assert!(!text.contains("claude"), "{text}");
    assert_eq!(cfg.effective()["claude"].origin, HarnessOrigin::Detected);
}

#[cfg(unix)]
#[test]
fn paths_are_checked_where_they_lead() {
    let Some(_home) = isolated_home("paths_are_checked_where_they_lead", &[".claude"]) else { return };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(MACHINE_FILE);
    let t = dir.path();
    fs::create_dir_all(t.join("s/inner")).unwrap();
    std::os::unix::fs::symlink(&_home, t.join("home-alias")).unwrap();
    std::os::unix::fs::symlink(t.join("s"), t.join("alias")).unwrap();
    let p = |rel: &str| Some(t.join(rel).to_string_lossy().into_owned());
    let set = |name: &str, skills: Option<String>, instructions: Option<String>| HarnessChange::Set {
        name: name.into(),
        harness: Harness { skills, instructions, ..Harness::default() },
    };
    edit(&path, set("a", p("s"), None)).unwrap();
    let refused = [
        set("x", Some("~/alias/../x".into()), None),
        set("x", p("s/../x"), None),
        set("x", p("home-alias"), None),
        set("x", Some("/".into()), None),
        // Nested in another harness's location, either way, or by an alias.
        set("x", p("s/inner"), None),
        set("x", Some(t.to_string_lossy().into_owned()), None),
        set("x", None, p("s/AGENTS.md")),
        set("x", p("alias/inner"), None),
        set("x", p("alias"), None),
        // Or within one harness.
        set("x", p("y"), p("y/AGENTS.md")),
        set("x", Some("~/.claude/skills/x".into()), None),
    ];
    for change in refused {
        assert!(edit(&path, change.clone()).is_err(), "{change:?}");
    }
    edit(&path, set("x", p("y/skills"), p("y/AGENTS.md"))).unwrap();

    // An entry written by hand is checked again whenever it is turned
    // on, but can always be turned off or removed.
    fs::write(&path, format!("[harnesses.bad]\nskills = {:?}\nenabled = false\n", t.join("s/../s/x"))).unwrap();
    assert!(edit(&path, HarnessChange::Enable { name: "bad".into() }).is_err());
    edit(&path, HarnessChange::Disable { name: "bad".into() }).unwrap();
    edit(&path, HarnessChange::Remove { name: "bad".into() }).unwrap();
    // So is a preset reset to paths that now overlap another harness.
    fs::write(
        &path,
        format!("[harnesses.claude]\nskills = {:?}\n[harnesses.mine]\nskills = \"~/.claude/skills\"\n", t.join("c")),
    )
    .unwrap();
    assert!(edit(&path, HarnessChange::Remove { name: "claude".into() }).is_err());
}

fn preset_harness(name: &str) -> Harness {
    MachineConfig::default().choices().into_iter().find(|choice| choice.name == name).unwrap().harness
}

/// A missing file is this host with no harness entries; anything but a
/// regular file of known keys is refused, naming the file.
#[test]
fn machine_toml_reports_malformed_fields_at_the_config_path() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join(MACHINE_FILE);
    let missing = MachineConfig::load(&path).unwrap();
    assert!(!missing.id.is_empty() && missing.harnesses.is_empty());
    #[cfg(unix)]
    {
        let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(MachineConfig::load(&path).unwrap_err().to_string().contains("not a regular file"));
        fs::remove_file(&path).unwrap();
    }
    for (text, detail) in [
        ("id = 17", "expected a string"),
        ("extra = true", "unknown field"),
        ("[notifications]\nenabled = 'yes'", "expected a boolean"),
        ("[notifications]\nother = true", "unknown field"),
        ("[harnesses.h]\nmode = 'link'", "unknown variant"),
        ("[harnesses.h]\ninstructions_format = 'file'", "unknown variant"),
        ("[harnesses.h]\nenabled = 1", "expected a boolean"),
        ("[harnesses.h]\nskill = '/tmp/skills'", "unknown field"),
        ("harnesses = []", "expected a map"),
    ] {
        fs::write(&path, text).unwrap();
        let error = MachineConfig::load(&path).unwrap_err();
        assert!(
            matches!(error, Error::Parse { path: ref at, ref message } if at == &path && message.contains(detail)),
            "{text}: {error}"
        );
    }
}

#[test]
fn invalid_harness_edits_name_the_problem_and_preserve_machine_toml() {
    let Some(_home) = isolated_home("invalid_harness_edits_name_the_problem_and_preserve_machine_toml", &[]) else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join(MACHINE_FILE);
    let before = "# mine\nid = 'box'\n";
    fs::write(&path, before).unwrap();
    let put = |name: &str, skills: Option<&str>| HarnessChange::Set {
        name: name.into(),
        harness: Harness { skills: skills.map(str::to_string), ..Harness::default() },
    };
    for (edit, detail) in [
        (put("Bad Name", Some("/somewhere")), "invalid harness name"),
        (put("work", None), "needs a skills directory, an instructions path or both"),
        (put("work", Some(" /home/skills")), "must be a plain path without surrounding spaces"),
        (put("work", Some("a/b")), "must be absolute or start with"),
        (put("work", Some("~/a/../b")), "must not contain"),
        (put("work", Some("~/")), "must not be the home directory"),
        (HarnessChange::Enable { name: "absent".into() }, "no harness `absent`"),
        (HarnessChange::Disable { name: "absent".into() }, "no harness `absent`"),
        (HarnessChange::Remove { name: "absent".into() }, "doesn't exist; nothing to remove"),
        (HarnessChange::Remove { name: "codex".into() }, "uses the built-in preset already"),
    ] {
        let error = MachineConfig::write_change(&path, &edit).unwrap_err();
        assert!(error.to_string().contains(detail), "{error}");
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
    }
}
