//! Periodic check of the baseline's imports against their upstream refs.
//!
//! The sources come from the pushed manifest, so operators can turn the check
//! off (`SK1LL_SERVER_IMPORT_CHECKS=off`) or limit it to some hosts
//! (`SK1LL_SERVER_IMPORT_HOSTS`). Imports it doesn't check are reported as
//! skipped, with the reason.

use std::sync::Arc;

use sk1ll_core::lock::LOCK_FILE;
use sk1ll_core::manifest::MANIFEST_FILE;
use sk1ll_core::source::resolve_url;
use sk1ll_core::{Lockfile, Manifest};
use sk1ll_proto::{ImportError, ImportSkipped, ImportUpdate, ImportsView, ServerMessage};

use crate::config::Config;
use crate::{Server, git, lock, now};

/// Checks now, then every `import_poll` and after each push. Daemons are
/// told when the updates, errors or skipped imports change; the message
/// carries the updates, and the GUI rereads
/// the whole view.
pub async fn poll(server: Arc<Server>) {
    loop {
        let view = check(&server).await;
        let changed = {
            let mut current = lock(&server.imports);
            let changed =
                current.updates != view.updates || current.errors != view.errors || current.skipped != view.skipped;
            *current = view.clone();
            changed
        };
        if changed {
            tracing::info!(
                "{} import(s) have upstream updates, {} failed and {} were skipped",
                view.updates.len(),
                view.errors.len(),
                view.skipped.len()
            );
            server.broadcast(&ServerMessage::ImportsChanged { updates: view.updates });
        }
        tokio::select! {
            () = tokio::time::sleep(server.config.import_poll) => {}
            () = server.imports_due.notified() => {}
        }
    }
}

async fn check(server: &Server) -> ImportsView {
    let mut view = ImportsView::default();
    if let Some(head) = server.head()
        && let Err(e) = check_at(server, &head, &mut view).await
    {
        view.errors.push(ImportError { name: MANIFEST_FILE.into(), message: format!("{e:#}") });
    }
    view.updates.sort();
    view.checked_at = Some(now());
    view
}

async fn check_at(server: &Server, head: &str, view: &mut ImportsView) -> anyhow::Result<()> {
    let repo = server.config.repo_dir();
    let Some(manifest) = git::show(&repo, head, MANIFEST_FILE).await? else {
        return Ok(());
    };
    let manifest: Manifest = toml::from_str(&manifest)?;
    let lockfile: Lockfile = match git::show(&repo, head, LOCK_FILE).await? {
        Some(text) => toml::from_str(&text)?,
        None => Lockfile::default(),
    };
    for (name, import) in manifest.import {
        // Not vendored yet, or pinned to a commit: nothing to compare.
        let Some(locked) = lockfile.skills.get(&name) else {
            continue;
        };
        if is_commit(&import.git_ref) {
            continue;
        }
        let url = resolve_url(&import.source);
        if let Some(reason) = skip_reason(&server.config, &url) {
            view.skipped.push(ImportSkipped { name, reason });
            continue;
        }
        // Redirects, url rewrites and ssh config could lead past the allowlist.
        let remote = git::RemoteCheck {
            protocols: &server.config.import_protocols,
            isolated: server.config.import_hosts.is_some(),
            ssh: &server.config.import_ssh,
        };
        match git::ls_remote(&url, &import.git_ref, &remote).await {
            Ok(Some(upstream)) if upstream != locked.commit => view.updates.push(ImportUpdate {
                name,
                source: import.source,
                git_ref: import.git_ref,
                locked: locked.commit.clone(),
                upstream,
            }),
            Ok(Some(_)) => {}
            Ok(None) => view.errors.push(ImportError { name, message: format!("ref `{}` not found", import.git_ref) }),
            Err(e) => view.errors.push(ImportError { name, message: format!("{e:#}") }),
        }
    }
    Ok(())
}

/// Why the settings keep the check away from `url`, if they do.
fn skip_reason(config: &Config, url: &str) -> Option<String> {
    if !config.import_checks {
        return Some("import checks are off on the server (SK1LL_SERVER_IMPORT_CHECKS=off)".into());
    }
    let hosts = config.import_hosts.as_ref()?;
    match import_host(url) {
        Ok(host) if hosts.contains(&host) => None,
        Ok(host) => Some(format!("host `{host}` is not in the server's SK1LL_SERVER_IMPORT_HOSTS")),
        Err(why) => Some(format!("the server allows only some hosts and can't tell this source's host: {why}")),
    }
}

/// Characters a host may contain before normalizing: ASCII letters, digits,
/// `-` and `.`, brackets and colons for IPv6, and non-ASCII for IDNs.
fn host_chars(raw: &str) -> bool {
    raw.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '[' | ']' | ':') || !c.is_ascii())
}

/// A host name or IP address as compared: lowercase, IDNs in punycode, IPv6
/// in brackets. Refuses anything with a port, userinfo, path, wildcard or
/// percent-encoding.
pub fn normalize_host(raw: &str) -> Result<String, String> {
    if raw.is_empty() || raw.starts_with('-') || !host_chars(raw) || raw.chars().any(char::is_whitespace) {
        return Err(format!("`{raw}` is not a host name or IP address"));
    }
    if raw.contains(':') && !(raw.starts_with('[') && raw.ends_with(']')) {
        return Err(format!("`{raw}` has a port or an unbracketed IPv6 address"));
    }
    url::Host::parse(raw).map(|h| h.to_string()).map_err(|e| format!("`{raw}`: {e}"))
}

/// The normalized host git would contact for `url`: a `scheme://` URL of a
/// network transport, or scp-like `[user@]host:path`. Anything else, and
/// anything git or curl might read differently, is refused: several `@`,
/// backslashes, percent escapes before the path (git decodes ssh URLs before
/// it finds the host, so `%2F` in the userinfo moves it), ssh user names
/// other than `[A-Za-z0-9._-]`, remote helpers (`ext::`), local paths,
/// non-ASCII hosts (write them as punycode) and hosts that only match after
/// normalizing.
pub fn import_host(url: &str) -> Result<String, String> {
    if url.chars().any(|c| c.is_whitespace() || c.is_control() || c == '\\') {
        return Err("the URL contains whitespace, control characters or a backslash".into());
    }
    let (authority, scp, ssh) = if let Some((scheme, rest)) = url.split_once("://") {
        let scheme = scheme.to_ascii_lowercase();
        if !matches!(scheme.as_str(), "https" | "http" | "ssh" | "git" | "git+ssh" | "ssh+git") {
            return Err(format!("`{scheme}://` is not a network transport the import check uses"));
        }
        let end = rest.find('/').unwrap_or(rest.len());
        (&rest[..end], false, scheme.contains("ssh"))
    } else if let Some(helper) = url.find("::").map(|i| &url[..i])
        && !helper.is_empty()
        && helper.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    {
        return Err(format!("`{helper}::` is a git remote helper"));
    } else {
        // git reads `host:path` as ssh when the first `:` comes before any `/`;
        // a bracketed host may hold colons.
        let mut depth = false;
        let colon = url.char_indices().find(|&(_, c)| {
            depth = (depth || c == '[') && c != ']';
            c == ':' && !depth
        });
        match colon {
            Some((i, _)) if !url[..i].contains('/') => (&url[..i], true, true),
            _ => return Err("the source is a local path".into()),
        }
    };
    if authority.contains(['?', '#']) {
        return Err("the URL's authority contains `?` or `#`".into());
    }
    if authority.contains('%') {
        return Err("the URL has a percent escape before its path".into());
    }
    let host_port = match authority.split('@').collect::<Vec<_>>().as_slice() {
        [host] => *host,
        [user, host] => {
            // ssh gets the user as is; keep it to a plain user name.
            let plain = !user.is_empty()
                && !user.starts_with('-')
                && user.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
            if ssh && !plain {
                return Err(format!("the ssh user `{user}` is not a plain user name ([A-Za-z0-9._-])"));
            }
            *host
        }
        _ => return Err("the URL has more than one `@`".into()),
    };
    let host = if host_port.starts_with('[') {
        let end = host_port.find(']').ok_or("the URL has an unclosed `[`")? + 1;
        check_port(&host_port[end..], scp)?;
        &host_port[..end]
    } else {
        match host_port.rsplit_once(':') {
            Some((host, port)) => {
                check_port(&format!(":{port}"), scp)?;
                host
            }
            None => host_port,
        }
    };
    if host.is_empty() {
        return Err("the URL has no host".into());
    }
    if !host.is_ascii() {
        return Err(format!("the host `{host}` isn't ASCII; write it in punycode (`xn--...`)"));
    }
    let normalized = normalize_host(host)?;
    if normalized != host.to_ascii_lowercase() {
        return Err(format!("the host `{host}` is ambiguous (it reads as `{normalized}`)"));
    }
    Ok(normalized)
}

/// What follows the host: nothing, or `:<digits>` in a `scheme://` URL.
fn check_port(rest: &str, scp: bool) -> Result<(), String> {
    match rest.strip_prefix(':') {
        None if rest.is_empty() => Ok(()),
        Some(port) if !scp && !port.is_empty() && port.len() <= 5 && port.bytes().all(|b| b.is_ascii_digit()) => Ok(()),
        _ => Err(format!("the URL has an invalid port `{rest}`")),
    }
}

fn is_commit(git_ref: &str) -> bool {
    git_ref.len() == 40 && git_ref.chars().all(|c| c.is_ascii_hexdigit())
}
