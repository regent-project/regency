//! Fetching a Git source (blocking gix; call from `spawn_blocking`): a bare
//! clone per source under the work directory, fetched on every poll; files
//! are read from the commit tree, no worktree.

use std::path::Path;

use gix::bstr::ByteSlice;

use crate::parse::File;

fn token_credentials(
    token: String,
) -> impl FnMut(
    gix::credentials::helper::Action,
) -> gix::Result<Option<gix::credentials::protocol::Outcome>>
+ 'static {
    move |action| match action {
        gix::credentials::helper::Action::Get(context) => {
            Ok(Some(gix::credentials::protocol::Outcome {
                identity: gix::sec::identity::Account {
                    username: "oauth2".to_string(),
                    password: token.clone(),
                    oauth_refresh_token: None,
                },
                next: context.into(),
            }))
        }
        _ => Ok(None),
    }
}

/// Fetch `branch` of `url` into `dir` (bare), returning the commit sha and
/// the files below `subpath`.
pub fn fetch(
    url: &str,
    branch: &str,
    subpath: &str,
    dir: &Path,
    token: Option<String>,
) -> Result<(String, Vec<File>), String> {
    let repo = if dir.join("HEAD").exists() {
        let repo = gix::open(dir).map_err(|e| format!("open {}: {e}", dir.display()))?;
        let remote = repo
            .find_remote("origin")
            .map_err(|e| format!("remote: {e}"))?
            // bare clones keep no fetch refspec: mirror the branches
            .with_refspecs(
                ["+refs/heads/*:refs/heads/*"],
                gix::remote::Direction::Fetch,
            )
            .map_err(|e| format!("refspec: {e}"))?;
        let mut connection = remote
            .connect(gix::remote::Direction::Fetch)
            .map_err(|e| format!("connect {url}: {e}"))?;
        if let Some(token) = token {
            connection.set_credentials(token_credentials(token));
        }
        connection
            .prepare_fetch(gix::progress::Discard, Default::default())
            .map_err(|e| format!("fetch {url}: {e}"))?
            .receive(gix::progress::Discard, &gix::interrupt::IS_INTERRUPTED)
            .map_err(|e| format!("fetch {url}: {e}"))?;
        repo
    } else {
        std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        let mut prepare =
            gix::prepare_clone_bare(url, dir).map_err(|e| format!("clone {url}: {e}"))?;
        if let Some(token) = token {
            prepare = prepare.configure_connection(move |connection| {
                connection.set_credentials(token_credentials(token.clone()));
                Ok(())
            });
        }
        let (repo, _) = prepare
            .fetch_only(gix::progress::Discard, &gix::interrupt::IS_INTERRUPTED)
            .map_err(|e| format!("clone {url}: {e}"))?;
        repo
    };

    // bare clones map remote branches to refs/heads/*
    let mut reference = repo
        .find_reference(&format!("refs/heads/{branch}"))
        .or_else(|_| repo.find_reference(&format!("refs/remotes/origin/{branch}")))
        .map_err(|e| format!("branch {branch}: {e}"))?;
    let commit = reference
        .peel_to_commit()
        .map_err(|e| format!("branch {branch}: {e}"))?;
    let sha = commit.id().to_string();
    let mut tree = commit.tree().map_err(|e| format!("tree: {e}"))?;
    let subpath = subpath.trim_matches('/');
    if !subpath.is_empty() {
        let entry = tree
            .peel_to_entry_by_path(subpath)
            .map_err(|e| format!("path {subpath}: {e}"))?
            .ok_or_else(|| format!("path {subpath} not found at {sha}"))?;
        tree = entry
            .object()
            .map_err(|e| format!("path {subpath}: {e}"))?
            .try_into_tree()
            .map_err(|_| format!("path {subpath} is not a directory"))?;
    }
    let mut files = Vec::new();
    for dir_name in ["inventories", "expected_states", "assignments"] {
        let Some(entry) = tree
            .clone()
            .peel_to_entry_by_path(dir_name)
            .map_err(|e| e.to_string())?
        else {
            continue;
        };
        let Ok(dir_tree) = entry.object().map_err(|e| e.to_string())?.try_into_tree() else {
            continue;
        };
        for item in dir_tree.iter() {
            let item = item.map_err(|e| e.to_string())?;
            if !item.mode().is_blob() {
                continue;
            }
            let name = item.filename().to_str_lossy().to_string();
            let blob = repo.find_object(item.oid()).map_err(|e| e.to_string())?;
            files.push(File {
                path: format!("{dir_name}/{name}"),
                content: blob.data.clone(),
            });
        }
    }
    Ok((sha, files))
}
