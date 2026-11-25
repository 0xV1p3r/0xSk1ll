//! Smoke test of `sk1ll_testkit::Machine` with the real binary.
#![cfg(unix)]

use sk1ll_testkit::{Machine, Skill};

#[test]
fn a_testkit_machine_syncs_its_repo_into_its_harness() {
    let mut machine = Machine::new();
    let harness = machine.harness("h");
    machine.repo.skill(Skill::new("pdf").body("PDFs.\n"));
    let out = machine.sk1ll(env!("CARGO_BIN_EXE_sk1ll"), &["sync"]).output().unwrap();
    assert!(out.status.success(), "{}", sk1ll_testkit::text(&out));
    assert!(sk1ll_testkit::read(&harness.join("pdf/SKILL.md")).ends_with("PDFs.\n"));
}
