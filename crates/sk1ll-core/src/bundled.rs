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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_documented_link_has_a_bundled_copy() {
        assert_eq!(for_link(SK1LL_SKILL_LINK), Some("sk1ll"));
        assert_eq!(for_link(&format!(" {SK1LL_SKILL_LINK}/ ")), Some("sk1ll"));
        assert_eq!(for_link("https://github.com/owner/repo/tree/main/skills/sk1ll"), None);
        assert!(skill("sk1ll").is_some_and(|s| s.starts_with("---\nname: sk1ll\n")));
    }

    #[test]
    fn the_bundled_copy_lands_in_personal_and_never_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(dir.path()).unwrap();
        repo.add_bundled("sk1ll", "sk1ll").unwrap();
        let written = std::fs::read_to_string(dir.path().join("personal/sk1ll/SKILL.md")).unwrap();
        assert_eq!(written, SK1LL_SKILL);
        assert!(repo.add_bundled("sk1ll", "sk1ll").unwrap_err().to_string().contains("taken"));
        assert!(repo.add_bundled("pdf", "pdf").is_err());
    }
}
