use super::*;
use std::sync::Arc;

use sk1ll_core::{ItemRef, PullOutcome, Repo, default_commit_message, machine_author};

use crate::client::{CommitRequest, CommitResponse, Event, PullResult, PushResult};
use crate::error::{Error, Result};
use crate::gitx;
use crate::state::PullState;

impl Daemon {
    /// After a Draft change, which may have cleared what stopped the last
    /// pull: a Blocked Machine pulls again, once per change of the Draft.
    pub(super) async fn retry_stopped_pull(self: &Arc<Self>) {
        if self.draft_moved.swap(false, std::sync::atomic::Ordering::SeqCst)
            && self.lifecycle().pull_stopped()
            && (self.is_connected() || self.is_standalone())
        {
            let _ = self.pull().await;
        }
    }

    /// Fetches the server's baseline and moves onto it (draft carried along),
    /// pushes queued commits, and reconciles. The fetch (or the first clone)
    /// and the push run without the engine mutex; moving onto what was
    /// fetched and the reconcile hold it. Each step under the mutex checks
    /// again that the daemon still manages the repo, and moves onto whatever
    /// was fetched last, whoever fetched it. A pull that moved nothing
    /// changed no Input, so its reconcile has nothing to do.
    pub(crate) async fn pull(self: &Arc<Self>) -> Result<PullResult> {
        let standalone = self.is_standalone();
        self.blocking(move |d, eng| {
            d.ensure_managed()?;
            if standalone {
                d.require_origin()?;
            }
            d.lock_repo(eng)
        })
        .await?;
        let result = match self.network(|d| d.fetch_baseline()).await {
            Ok(()) => {
                self.blocking(|d, eng| {
                    d.ensure_managed()?;
                    d.lock_repo(eng)?;
                    d.pull_inner(eng)
                })
                .await
            }
            Err(e) => Err(e),
        };
        let result = match result {
            Ok(r) if !standalone && !matches!(r, PullResult::Blocked { .. }) => self.push_pending().await.map(|_| r),
            result => result,
        };
        self.blocking(move |d, eng| {
            if d.idle() {
                return result;
            }
            d.reconcile(eng)?;
            match &result {
                Ok(r) => {
                    if let PullResult::Updated { to, .. } = r {
                        tracing::info!("baseline is now {to}");
                    }
                    if !matches!(r, PullResult::Blocked { .. }) {
                        d.update(|v| v.last_error = None);
                    }
                }
                Err(e) => d.set_error(format!("pull: {e}")),
            }
            result
        })
        .await
    }

    /// Without the engine mutex: clones the repo if it isn't there yet (a
    /// clone has fetched), else fetches origin's branches. Neither moves the
    /// working tree, `HEAD` or the index of an existing repo.
    pub(super) fn fetch_baseline(&self) -> Result<()> {
        let root = self.repo_root();
        if !root.join(".git").exists() {
            gitx::ensure_clone(root, &self.ident.joined()?.repo_url(), &[])?;
            return Ok(());
        }
        Ok(Repo::open_git_only(root)?.fetch()?)
    }

    /// The local half of a pull: moves onto the baseline fetched last.
    pub(super) fn pull_inner(&self, eng: &mut Engine) -> Result<PullResult> {
        let root = self.repo_root().to_path_buf();
        let repo = Repo::open_git_only(&root)?;
        let first = if repo.pull_pending()? {
            None
        } else {
            match gitx::rev(&root, "HEAD")? {
                None => Some(gitx::adopt_first_baseline(&root)?),
                Some(_) => gitx::adopt_first_baseline_over_init(&root)?,
            }
        };
        let (result, pull) = if let Some(first) = first {
            match first {
                gitx::FirstBaseline::Adopted(to) => (PullResult::Updated { from: None, to }, PullState::Clear),
                gitx::FirstBaseline::Empty => (PullResult::UpToDate, PullState::Clear),
                gitx::FirstBaseline::Blocked { baseline, items } => (
                    PullResult::Blocked { baseline: baseline.clone(), items: items.clone(), pending: false },
                    PullState::Blocked { baseline, items },
                ),
            }
        } else {
            let head = gitx::rev(&root, "HEAD")?;
            let lock = eng.lock.as_ref().ok_or_else(|| Error::Conflict(format!("{} is not locked", root.display())))?;
            match repo.pull_fetched(lock) {
                Ok(PullOutcome::UpToDate) => (PullResult::UpToDate, PullState::Clear),
                Ok(PullOutcome::Updated { from, to }) => {
                    (PullResult::Updated { from: Some(from), to }, PullState::Clear)
                }
                Ok(PullOutcome::Blocked { baseline, items }) => {
                    tracing::warn!("blocked: baseline {baseline} clashes with the draft");
                    let pending = repo.pull_pending()?;
                    let pull = if pending {
                        PullState::Pending { baseline: baseline.clone(), items: items.clone() }
                    } else {
                        PullState::Blocked { baseline: baseline.clone(), items: items.clone() }
                    };
                    (PullResult::Blocked { baseline, items, pending }, pull)
                }
                // The pull went through; a save made to a file it replaced
                // is in the recovery area, which the report lists as a conflict.
                Err(e @ sk1ll_core::Error::Displaced { .. }) if !repo.pull_pending()? => {
                    tracing::warn!("{e}");
                    let result = match gitx::rev(&root, "HEAD")? {
                        Some(to) if Some(&to) != head.as_ref() => PullResult::Updated { from: head, to },
                        _ => PullResult::UpToDate,
                    };
                    (result, PullState::Clear)
                }
                Err(e) => return Err(e.into()),
            }
        };
        let remote = gitx::remote_head(&root)?;
        self.note(|lc| {
            lc.set_pull(pull);
            if remote.is_some() {
                lc.set_server_baseline(remote);
            }
        });
        Ok(result)
    }

    /// Pushes committed commits the server doesn't have. What to push is
    /// checked under the engine mutex and pushed without it: exactly that
    /// commit, so one made meanwhile waits for the next push. A push the
    /// server rejected as not fast-forward is retried once after pulling its
    /// baseline; any other failure (unreachable, timed out) is returned right
    /// away.
    pub(super) async fn push_pending(self: &Arc<Self>) -> Result<bool> {
        let mut retried = false;
        loop {
            let commit = self
                .blocking(|d, _| {
                    d.ensure_managed()?;
                    let root = d.repo_root();
                    if !gitx::has_unpushed(root)? {
                        return Ok(None);
                    }
                    Ok(Repo::open_git_only(root)?.prepare_push()?)
                })
                .await?;
            let Some(commit) = commit else { return Ok(false) };
            let pushed = self
                .network(move |d| {
                    let root = d.repo_root();
                    let pushed = Repo::open_git_only(root).and_then(|repo| repo.push_commit(&commit));
                    if pushed.is_ok() {
                        let remote = gitx::remote_head(root)?;
                        d.note(|lc| lc.set_server_baseline(remote));
                    }
                    Ok(pushed)
                })
                .await?;
            match pushed {
                Ok(()) => {
                    tracing::info!("pushed to the server");
                    return Ok(true);
                }
                Err(first) if !retried && is_rejected_push(&first) => {
                    tracing::info!("push rejected ({first}); pulling first");
                    retried = true;
                    self.network(|d| d.fetch_baseline()).await?;
                    let pulled = self
                        .blocking(|d, eng| {
                            d.ensure_managed()?;
                            d.pull_inner(eng)
                        })
                        .await?;
                    if let PullResult::Blocked { items, .. } = pulled {
                        let names: Vec<String> = items.iter().map(ToString::to_string).collect();
                        return Err(Error::Conflict(format!(
                            "can't push: the server's baseline clashes with {}; commit or restore them",
                            names.join(", ")
                        )));
                    }
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// `POST /push`: pushes committed commits now.
    pub(crate) async fn push(self: &Arc<Self>) -> Result<PushResult> {
        self.blocking(|d, eng| {
            d.ensure_managed()?;
            d.lock_repo(eng)?;
            if d.is_standalone() {
                d.require_origin()?;
            }
            Ok(())
        })
        .await?;
        let result = self.push_pending().await;
        self.blocking(|d, eng| d.reconcile(eng)).await?;
        match result {
            Ok(pushed) => Ok(PushResult { pushed }),
            Err(Error::Internal(e)) => {
                Err(Error::Unavailable(format!("push: {}", crate::auth::redact(&format!("{e:#}")))))
            }
            Err(e) => Err(e),
        }
    }

    /// On (re)connecting: push what's queued and pull.
    pub(crate) async fn on_connected(self: &Arc<Self>) -> Result<()> {
        if self.idle() {
            return Ok(());
        }
        self.pull().await.map(drop)
    }

    /// Commits under the engine mutex; a joined daemon then pushes, and the
    /// state is reconciled once the push is done.
    pub(crate) async fn commit(self: &Arc<Self>, req: CommitRequest) -> Result<CommitResponse> {
        let mut response = self.blocking(move |d, eng| d.commit_locally(eng, req)).await?;
        if !self.is_standalone() {
            if self.is_connected() {
                match self.push_pending().await {
                    Ok(_) => response.pushed = true,
                    Err(e) => response.push_error = Some(crate::auth::redact(&e.to_string())),
                }
            } else {
                response.push_error = Some("offline: queued until the server is reachable".to_string());
            }
        }
        let push_error = response.push_error.clone();
        self.blocking(move |d, eng| {
            d.reconcile(eng)?;
            if let Some(e) = &push_error {
                d.set_error(format!("push: {e}"));
            }
            Ok(())
        })
        .await?;
        self.retry_stopped_pull().await;
        Ok(response)
    }

    pub(super) fn commit_locally(&self, eng: &mut Engine, req: CommitRequest) -> Result<CommitResponse> {
        self.ensure_managed()?;
        self.lock_repo(eng)?;
        let repo = Repo::open_git_only(self.repo_root())?;
        let draft = repo.draft()?;
        let items = req.items.iter().map(|i| ItemRef::resolve(i, &draft)).collect::<Result<Vec<_>, _>>()?;
        if items.is_empty() {
            return Err(Error::BadRequest("name the draft items to commit".into()));
        }
        let id = self.ident.machine_id().to_string();
        let message =
            req.message.filter(|m| !m.trim().is_empty()).unwrap_or_else(|| default_commit_message(&items, &id));
        let author = match req.author {
            Some(a) => Some(a),
            None if !repo.has_git_identity()? => Some(machine_author(&id)),
            None => None,
        };
        let sha = repo.commit(&items, &message, author.as_deref())?;
        tracing::info!("committed {} as {sha}", items.iter().map(ToString::to_string).collect::<Vec<_>>().join(", "));
        Ok(CommitResponse { sha, items, message, pushed: false, push_error: None })
    }

    /// The server's baseline is now `baseline`: pull unless already on it. A
    /// nudge for the baseline the machine is on (the server sends one on every
    /// connect) changes no Input, so its reconcile has nothing to do.
    pub(crate) async fn nudge(self: &Arc<Self>, baseline: String) -> Result<()> {
        let pull = self
            .blocking(move |d, eng| {
                if d.idle() {
                    return Ok(false);
                }
                d.note(|lc| lc.set_server_baseline(Some(baseline.clone())));
                d.event(Event::Fleet);
                let current = gitx::baseline(d.repo_root()).unwrap_or(None);
                if current.as_deref() != Some(baseline.as_str()) || d.lifecycle().pull_stopped() {
                    return Ok(true);
                }
                d.reconcile(eng)?;
                Ok(false)
            })
            .await?;
        if pull {
            self.pull().await?;
        }
        Ok(())
    }
}

/// Whether a push failed because the server has commits we don't (worth a pull
/// and one retry), as opposed to the server being unreachable.
pub(super) fn is_rejected_push(e: &sk1ll_core::Error) -> bool {
    let text = e.to_string();
    ["[rejected]", "non-fast-forward", "fetch first", "failed to push some refs"].iter().any(|m| text.contains(m))
        && !text.contains("unable to access")
}
