use anyhow::{Context as _, Result, ensure};
use buffer_diff::BufferDiff;
use futures::future::join_all;
use git::repository::RepoPath;
use gpui::{App, AppContext as _, Entity, SharedString, Task, Window};
use language::{Buffer, BufferSnapshot};
use project::{
    Project,
    git_store::{
        Repository,
        branch_diff::{BranchDiff, DiffBase},
    },
};

pub struct BranchReviewSnapshot {
    pub branch: SharedString,
    pub files: Vec<BranchReviewFile>,
    pub omitted_files: Vec<(RepoPath, String)>,
    project: Entity<Project>,
    repository: Entity<Repository>,
    repository_scan_id: u64,
    sources: Vec<(Entity<Buffer>, BufferSnapshot)>,
}

pub struct BranchReviewFile {
    pub path: RepoPath,
    pub base_text: Option<String>,
    pub current_text: Option<String>,
    pub source_snapshot: BufferSnapshot,
    pub source_diff: Entity<BufferDiff>,
}

impl BranchReviewSnapshot {
    pub fn load(
        project: Entity<Project>,
        repository: Entity<Repository>,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Self>> {
        window.spawn(cx, async move |cx| {
            repository
                .update(cx, |repository, _| repository.barrier())
                .await
                .context("Could not load the repository for guided review")?;
            let (branch, repository_scan_id, branch_diff) = cx.update(|window, cx| {
                ensure!(
                    project.read(cx).is_local(),
                    "Guided review requires a local project"
                );
                let state = repository.read(cx);
                ensure!(
                    state.head_commit.is_some(),
                    "Commit your initial changes before starting a guided review"
                );
                let branch = state
                    .branch
                    .as_ref()
                    .context("Check out a branch before starting a guided review")?
                    .ref_name
                    .clone();
                let scan_id = state.scan_id;
                let branch_diff = cx.new(|cx| {
                    BranchDiff::new(
                        DiffBase::Merge {
                            base_ref: "refs/heads/dev".into(),
                        },
                        project.clone(),
                        window,
                        cx,
                    )
                });
                branch_diff.update(cx, |diff, cx| diff.set_repo(Some(repository.clone()), cx));
                anyhow::Ok((branch, scan_id, branch_diff))
            })??;
            BranchDiff::reload_tree_diff(branch_diff.downgrade(), cx)
                .await
                .context("Could not compare this branch against dev")?;
            let entries = branch_diff.update(cx, |diff, cx| {
                let paths = dirty_buffers(project.read(cx), &repository, cx).map(|(_, path)| path);
                diff.load_buffers_including(paths.collect::<Vec<_>>(), cx)
            });
            let buffers = join_all(
                entries
                    .into_iter()
                    .map(|entry| async move { (entry.repo_path, entry.load.await) }),
            )
            .await;

            cx.update(|_, cx| {
                let mut files = Vec::new();
                let mut omitted_files = Vec::new();
                let mut sources = Vec::new();
                for (path, result) in buffers {
                    let (source, diff) = match result {
                        Ok(loaded) => loaded,
                        Err(error) => {
                            omitted_files.push((path, format!("{error:#}")));
                            continue;
                        }
                    };
                    let buffer = source.read(cx);
                    let snapshot = buffer.snapshot();
                    let exists = buffer.file().is_some_and(|file| file.disk_state().exists())
                        || buffer.has_unsaved_edits();
                    let base_text = diff.read(cx).base_text_string(cx);
                    let current_text = exists.then(|| snapshot.text());
                    if base_text != current_text {
                        files.push(BranchReviewFile {
                            path,
                            base_text,
                            current_text,
                            source_snapshot: snapshot.clone(),
                            source_diff: diff,
                        });
                    }
                    sources.push((source, snapshot));
                }
                files.sort_by(|left, right| left.path.cmp(&right.path));
                omitted_files.sort_by(|left, right| left.0.cmp(&right.0));
                let snapshot = Self {
                    branch,
                    files,
                    omitted_files,
                    project,
                    repository,
                    repository_scan_id,
                    sources,
                };
                ensure!(
                    snapshot.is_current(cx),
                    "Changes were updated while loading. Regenerate the guided review."
                );
                Ok(snapshot)
            })?
        })
    }

    pub fn is_current(&self, cx: &App) -> bool {
        self.repository.read(cx).scan_id == self.repository_scan_id
            && self.sources.iter().all(|(source, snapshot)| {
                let source = source.read(cx);
                source.version() == *snapshot.version()
                    && source.file().map(|file| file.disk_state())
                        == snapshot.file().map(|file| file.disk_state())
            })
            && dirty_buffers(self.project.read(cx), &self.repository, cx)
                .all(|(buffer, _)| self.sources.iter().any(|(source, _)| source == &buffer))
    }
}

fn dirty_buffers<'a>(
    project: &'a Project,
    repository: &'a Entity<Repository>,
    cx: &'a App,
) -> impl Iterator<Item = (Entity<Buffer>, RepoPath)> + 'a {
    project.opened_buffers(cx).into_iter().filter_map(|buffer| {
        if !buffer.read(cx).is_dirty() {
            return None;
        }
        let (buffer_repository, path) = project
            .git_store()
            .read(cx)
            .repository_and_path_for_buffer_id(buffer.read(cx).remote_id(), cx)?;
        (buffer_repository == *repository).then_some((buffer, path))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use gpui::TestAppContext;
    use serde_json::json;
    use settings::SettingsStore;
    use std::path::Path;
    use util::path;

    #[gpui::test]
    async fn captures_branch_and_local_changes_and_regenerates(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/review"),
            json!({
                ".git": {},
                "committed.txt": "branch change\n",
                "local.txt": "working tree\n",
                "new.txt": "untracked\n",
                "empty.txt": "",
                "reverted.txt": "original\n",
                "document.pdf": "%PDF-1.7\nbinary fixture\n",
                "workbook.xlsx": "PK\u{3}\u{4}binary fixture",
            }),
        )
        .await;
        let git_path = Path::new(path!("/review/.git"));
        fs.set_branch_name(git_path, Some("feature"));
        fs.set_head_for_repo(
            git_path,
            &[
                ("committed.txt", "branch change\n".into()),
                ("local.txt", "original\n".into()),
                ("deleted.txt", "original\n".into()),
                ("reverted.txt", "branch change\n".into()),
            ],
            "branch-head",
        );
        fs.set_index_for_repo(
            git_path,
            &[
                ("committed.txt", "branch change\n".into()),
                ("local.txt", "staged\n".into()),
                ("reverted.txt", "branch change\n".into()),
            ],
        );
        fs.set_merge_base_content_for_repo(
            git_path,
            &[
                ("committed.txt", "original\n".into()),
                ("local.txt", "original\n".into()),
                ("deleted.txt", "original\n".into()),
                ("reverted.txt", "original\n".into()),
            ],
        );
        let project = Project::test(fs.clone(), [Path::new(path!("/review"))], cx).await;
        let repository = project.read_with(cx, |project, cx| {
            project.active_repository(cx).expect("review repository")
        });
        let cx = cx.add_empty_window();
        cx.run_until_parked();
        let snapshot = cx
            .update(|window, cx| {
                BranchReviewSnapshot::load(project.clone(), repository.clone(), window, cx)
            })
            .await
            .expect("load branch review");
        assert_eq!(snapshot.branch, "refs/heads/feature");
        assert_eq!(
            snapshot
                .omitted_files
                .iter()
                .map(|(path, reason)| (path.as_unix_str(), reason.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("document.pdf", "Binary files are not supported"),
                ("workbook.xlsx", "Binary files are not supported"),
            ]
        );
        let files = snapshot
            .files
            .iter()
            .map(|file| {
                (
                    file.path.as_unix_str(),
                    file.base_text.as_deref(),
                    file.current_text.as_deref(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            files,
            vec![
                ("committed.txt", Some("original\n"), Some("branch change\n")),
                ("deleted.txt", Some("original\n"), None),
                ("empty.txt", None, Some("")),
                ("local.txt", Some("original\n"), Some("working tree\n")),
                ("new.txt", None, Some("untracked\n")),
            ]
        );
        assert!(cx.read(|cx| snapshot.is_current(cx)));

        let local = project
            .update(cx, |project, cx| {
                project.open_local_buffer(path!("/review/local.txt"), cx)
            })
            .await
            .expect("open local changes");
        local.update(cx, |buffer, cx| buffer.set_text("unsaved edit\n", cx));
        assert!(!cx.read(|cx| snapshot.is_current(cx)));
        let regenerated = cx
            .update(|window, cx| {
                BranchReviewSnapshot::load(project.clone(), repository.clone(), window, cx)
            })
            .await
            .expect("regenerate branch review");
        assert_eq!(
            regenerated
                .files
                .iter()
                .find(|file| file.path.as_unix_str() == "local.txt")
                .expect("regenerated local changes")
                .current_text
                .as_deref(),
            Some("unsaved edit\n")
        );
        assert!(cx.read(|cx| regenerated.is_current(cx)));

        let unsaved = project
            .update(cx, |project, cx| {
                project.open_local_buffer(path!("/review/unsaved.txt"), cx)
            })
            .await
            .expect("open new buffer");
        unsaved.update(cx, |buffer, cx| buffer.set_text("new unsaved file\n", cx));
        assert!(!cx.read(|cx| regenerated.is_current(cx)));
        let regenerated = cx
            .update(|window, cx| {
                BranchReviewSnapshot::load(project.clone(), repository.clone(), window, cx)
            })
            .await
            .expect("include new unsaved buffer");
        let unsaved = regenerated
            .files
            .iter()
            .find(|file| file.path.as_unix_str() == "unsaved.txt")
            .expect("unsaved file in snapshot");
        assert_eq!(unsaved.base_text, None);
        assert_eq!(unsaved.current_text.as_deref(), Some("new unsaved file\n"));
        assert!(cx.read(|cx| regenerated.is_current(cx)));

        fs.set_branch_name(git_path, Some("another-branch"));
        cx.run_until_parked();
        assert!(!cx.read(|cx| regenerated.is_current(cx)));
    }
}
