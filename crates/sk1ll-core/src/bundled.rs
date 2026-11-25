//! Skills shipped inside the binary, so they can be added when their
//! upstream can't be fetched (offline, or no access to the repo).

use crate::{Repo, Result};

/// The link the docs and `sk1ll setup` give for the sk1ll skill.
pub const SK1LL_SKILL_LINK: &str = "https://github.com/0xV1p3r/0xSk1ll/tree/main/skills/sk1ll";

const SK1LL_SKILL: &str = include_str!("../../../skills/sk1ll/SKILL.md");

/// The bundled skill named `name`: its `SKILL.md`.
pub fn skill(name: &str) -> Option<&'static str> {
    (name == "sk1ll").then_some(SK1LL_SKILL)
}

/// The bundled skill a link leads to, if it is one of the links above.
pub fn for_link(link: &str) -> Option<&'static str> {
    let link = link.trim().trim_end_matches('/');
    link.eq_ignore_ascii_case(SK1LL_SKILL_LINK).then_some("sk1ll")
}

impl Repo {
    /// Adds the bundled skill `bundled` as the personal skill `name` in the
    /// Draft. It is a copy, not an import: it doesn't follow upstream.
    pub fn add_bundled(&self, bundled: &str, name: &str) -> Result<()> {
        let body =
            skill(bundled).ok_or_else(|| crate::Error::Invalid(format!("no bundled skill named `{bundled}`")))?;
        self.create_item_with("skill", name, body)
    }
}
