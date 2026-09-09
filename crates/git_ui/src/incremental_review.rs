use anyhow::{Context as _, Result, anyhow, ensure};
use buffer_diff::{BufferDiff, DiffHunk};
use collections::{BTreeMap, HashMap};
use db::kvp::KeyValueStore;
use editor::{Editor, EditorEvent, MultiBuffer, multibuffer_context_lines};
use git::repository::RepoPath;
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, Font,
    Subscription, Task, Window, actions,
};
use language::{Buffer, BufferEvent, BufferSnapshot, Capability, HighlightedText, OffsetRangeExt};
use multi_buffer::{PathKey, ToPoint as _};
use project::{
    Project,
    git_store::{
        Repository,
        branch_diff::{BranchDiff, BranchDiffEvent, DiffBase},
    },
};
use serde::{Deserialize, Serialize};
use std::{
    any::{Any, TypeId},
    ops::Range,
    path::PathBuf,
    sync::Arc,
};
use ui::{Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{
    Item, ItemHandle as _, ItemNavHistory, ToolbarItemLocation, Workspace,
    item::{ItemEvent, SaveOptions, TabContentParams},
    notifications::NotifyTaskExt as _,
    searchable::SearchableItemHandle,
};

actions!(
    review,
    [
        /// Opens a persistent, block-by-block review of local changes without staging them.
        OpenIncrementalReview,
        /// Marks the selected change as reviewed and moves to the next change.
        MarkReviewed,
    ]
);

#[derive(Serialize, Deserialize)]
struct ReviewState {
    base_commit: String,
    baselines: BTreeMap<String, String>,
}

#[derive(Clone)]
struct ReviewedHunk {
    snapshot: BufferSnapshot,
    position: language::Anchor,
    base_text: Arc<str>,
    base_range: Range<usize>,
    replacement: String,
    deleted: bool,
}

struct ReviewBuffer {
    source: Entity<Buffer>,
    buffer: Entity<Buffer>,
    diff: Entity<BufferDiff>,
    base_text: Arc<str>,
    updating: bool,
    _subscription: Subscription,
    _update_task: Task<()>,
}

impl ReviewBuffer {
    fn new(source: Entity<Buffer>, base_text: String, cx: &mut Context<Self>) -> Self {
        let snapshot = source.read(cx).snapshot();
        let diff = cx.new(|cx| BufferDiff::new(&snapshot.text, cx));
        let subscription = cx.subscribe(&source, |this, _, event, cx| {
            if matches!(
                event,
                BufferEvent::Edited { .. }
                    | BufferEvent::Reloaded
                    | BufferEvent::FileHandleChanged
                    | BufferEvent::LanguageChanged(_)
            ) {
                this.refresh(cx);
            }
        });
        let mut this = Self {
            buffer: source.clone(),
            source,
            diff,
            base_text: base_text.into(),
            updating: false,
            _subscription: subscription,
            _update_task: Task::ready(()),
        };
        this.refresh(cx);
        this
    }

    fn is_deleted(&self, cx: &App) -> bool {
        self.source
            .read(cx)
            .file()
            .is_some_and(|file| file.disk_state().is_deleted())
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.updating = true;
        cx.notify();
        if self.is_deleted(cx) {
            if self.buffer == self.source {
                let file = self.source.read(cx).file().cloned();
                let language = self.source.read(cx).language().cloned();
                self.buffer = cx.new(|cx| {
                    let mut buffer = Buffer::local("", cx);
                    if let Some(file) = file {
                        buffer.file_updated(file, cx);
                    }
                    buffer.set_language(language, cx);
                    buffer.set_capability(Capability::ReadOnly, cx);
                    buffer
                });
                self.diff = cx.new(|cx| BufferDiff::new(&self.buffer.read(cx).text_snapshot(), cx));
            }
        } else if self.buffer != self.source {
            self.buffer = self.source.clone();
            self.diff = cx.new(|cx| BufferDiff::new(&self.buffer.read(cx).text_snapshot(), cx));
        }
        let snapshot = self.buffer.read(cx).snapshot();
        let base_text = self.base_text.clone();
        let diff = self.diff.clone();
        let update = diff.update(cx, |diff, cx| {
            diff.update_diff(
                snapshot.text.clone(),
                Some(base_text),
                Some(true),
                snapshot.language().cloned(),
                cx,
            )
        });
        self._update_task = cx.spawn(async move |this, cx| {
            let update = update.await;
            let task = diff.update(cx, |diff, cx| diff.set_snapshot(update, &snapshot.text, cx));
            task.await;
            this.update(cx, |this, cx| {
                this.updating = false;
                cx.notify();
            })
            .log_err();
        });
    }

    fn capture(&self, hunk: &DiffHunk, cx: &App) -> Option<ReviewedHunk> {
        let snapshot = self.buffer.read(cx).snapshot();
        let diff = self.diff.read(cx).snapshot(cx);
        if diff.buffer_version() != snapshot.version()
            || diff.base_text().text() != &*self.base_text
        {
            return None;
        }
        let text = snapshot.text();
        let range = hunk.buffer_range.to_offset(&snapshot);
        Some(ReviewedHunk {
            replacement: text.get(range)?.to_owned(),
            position: hunk.buffer_range.start,
            snapshot,
            base_text: self.base_text.clone(),
            base_range: hunk.diff_base_byte_range.clone(),
            deleted: self.is_deleted(cx),
        })
    }

    fn mark_reviewed(&mut self, hunk: &ReviewedHunk, cx: &mut Context<Self>) -> Result<()> {
        ensure!(
            self.buffer.read(cx).version() == *hunk.snapshot.version()
                && self.base_text == hunk.base_text
                && self.is_deleted(cx) == hunk.deleted,
            "This block changed while you were reviewing it. Review the updated block before checking it off."
        );
        let mut base_text = self.base_text.to_string();
        ensure!(
            base_text.get(hunk.base_range.clone()).is_some(),
            "The review baseline changed"
        );
        base_text.replace_range(hunk.base_range.clone(), &hunk.replacement);
        self.base_text = base_text.into();
        self.refresh(cx);
        Ok(())
    }
}

pub(super) fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, _: &OpenIncrementalReview, window, cx| {
        IncrementalReview::open(workspace, window, cx).detach_and_notify_err(
            workspace.weak_handle(),
            window,
            cx,
        );
    });
}

pub fn open_for_repository(
    workspace: &mut Workspace,
    repository_path: PathBuf,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Task<Result<()>> {
    let project = workspace.project().clone();
    let scans_complete = workspace.worktree_scans_complete(cx);
    cx.spawn_in(window, async move |workspace, cx| {
        scans_complete.await;
        let repository = project
            .read_with(cx, |project, cx| {
                project
                    .repositories(cx)
                    .values()
                    .find(|repository| {
                        repository.read(cx).work_directory_abs_path.as_ref() == repository_path
                    })
                    .cloned()
            })
            .with_context(|| format!("No Git repository found at {}", repository_path.display()))?;
        repository
            .update(cx, |repository, _| repository.barrier())
            .await
            .context("Could not load the Git repository for review")?;
        workspace
            .update_in(cx, |workspace, window, cx| {
                IncrementalReview::open_repository(workspace, repository, window, cx)
            })?
            .await
    })
}

struct IncrementalReview {
    editor: Entity<Editor>,
    multibuffer: Entity<MultiBuffer>,
    branch_diff: Entity<BranchDiff>,
    state: ReviewState,
    storage_key: String,
    buffers: HashMap<String, Entity<ReviewBuffer>>,
    focus_handle: FocusHandle,
    error: Option<String>,
    loading: bool,
    _subscriptions: Vec<Subscription>,
    _refresh_task: Task<()>,
    _persist_task: Option<Task<()>>,
}

impl IncrementalReview {
    fn repository_key(repository: &project::git_store::Repository) -> Option<String> {
        let commit = repository.head_commit.as_ref()?;
        let branch = repository
            .branch
            .as_ref()
            .map(|branch| branch.ref_name.as_ref())
            .unwrap_or(&commit.sha);
        Some(format!(
            "incremental-review:{}:{branch}",
            repository.work_directory_abs_path.display()
        ))
    }

    fn branch_changed(&self, cx: &App) -> bool {
        self.branch_diff
            .read(cx)
            .repo()
            .and_then(|repo| Self::repository_key(repo.read(cx)))
            .as_ref()
            != Some(&self.storage_key)
    }

    fn open(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Task<Result<()>> {
        let Some(repo) = workspace.project().read(cx).active_repository(cx) else {
            return Task::ready(Err(anyhow!(
                "Open a local Git repository to review changes"
            )));
        };
        Self::open_repository(workspace, repo, window, cx)
    }

    fn open_repository(
        workspace: &mut Workspace,
        repo: Entity<Repository>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Task<Result<()>> {
        let project = workspace.project().clone();
        if !project.read(cx).is_local() {
            return Task::ready(Err(anyhow!(
                "Incremental review currently supports local projects"
            )));
        }
        let repository = repo.read(cx);
        let Some((commit, storage_key)) = repository
            .head_commit
            .as_ref()
            .zip(Self::repository_key(repository))
        else {
            return Task::ready(Err(anyhow!(
                "The repository needs an initial commit before starting a review"
            )));
        };
        let existing = workspace
            .items_of_type::<Self>(cx)
            .find(|item| item.read(cx).storage_key == storage_key);
        if let Some(existing) = existing {
            workspace.activate_item(&existing, true, true, window, cx);
            return Task::ready(Ok(()));
        }
        let state = match KeyValueStore::global(cx)
            .read_kvp(&storage_key)
            .and_then(|serialized| {
                serialized
                    .map(|serialized| {
                        serde_json::from_str(&serialized)
                            .context("Could not read saved review state")
                    })
                    .transpose()
            }) {
            Ok(Some(state)) => state,
            Ok(None) => ReviewState {
                base_commit: commit.sha.to_string(),
                baselines: BTreeMap::default(),
            },
            Err(error) => return Task::ready(Err(error)),
        };
        let branch_diff = cx.new(|cx| {
            BranchDiff::new(
                DiffBase::Revision {
                    base_ref: state.base_commit.clone().into(),
                },
                project.clone(),
                window,
                cx,
            )
        });
        branch_diff.update(cx, |branch_diff, cx| branch_diff.set_repo(Some(repo), cx));
        let workspace = workspace.weak_handle();
        window.spawn(cx, async move |cx| {
            BranchDiff::reload_tree_diff(branch_diff.downgrade(), cx).await?;
            workspace.update_in(cx, |workspace, window, cx| {
                let item =
                    cx.new(|cx| Self::new(project, branch_diff, state, storage_key, window, cx));
                workspace.add_item_to_center(Box::new(item), window, cx);
            })?;
            Ok(())
        })
    }

    fn new(
        project: Entity<Project>,
        branch_diff: Entity<BranchDiff>,
        state: ReviewState,
        storage_key: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let multibuffer = cx.new(|cx| {
            let mut buffer = MultiBuffer::new(Capability::ReadOnly);
            buffer.set_all_diff_hunks_expanded(cx);
            buffer
        });
        let review = cx.weak_entity();
        let editor = cx.new(|cx| {
            let mut editor =
                Editor::for_multibuffer(multibuffer.clone(), Some(project), window, cx);
            editor.start_temporary_diff_override();
            editor.disable_diagnostics(cx);
            editor.set_expand_all_diff_hunks(cx);
            editor.set_render_diff_hunk_controls(
                Arc::new(move |row, _, range, _, line_height, editor, _, cx| {
                    let Some(review) = review.upgrade() else {
                        return gpui::Empty.into_any_element();
                    };
                    let Some((path, captured)) =
                        review.read(cx).capture_at(editor, range.start, cx)
                    else {
                        return gpui::Empty.into_any_element();
                    };
                    h_flex()
                        .h(line_height)
                        .px_1()
                        .bg(cx.theme().colors().editor_background)
                        .block_mouse_except_scroll()
                        .child(
                            Button::new(("mark-reviewed", row as u64), "Reviewed")
                                .start_icon(Icon::new(IconName::Check).size(IconSize::Small))
                                .tooltip(Tooltip::text(
                                    "Mark this block reviewed and go to the next change",
                                ))
                                .on_click(move |_, window, cx| {
                                    review.update(cx, |review, cx| {
                                        review.mark_hunk(&path, &captured, window, cx)
                                    });
                                }),
                        )
                        .into_any_element()
                }),
                cx,
            );
            editor
        });
        let subscription = cx.subscribe_in(&branch_diff, window, |this, _, event, window, cx| {
            if matches!(event, BranchDiffEvent::FileListChanged) {
                this.refresh(window, cx);
            }
        });
        let editor_subscription = cx.subscribe(&editor, |_, _, event: &EditorEvent, cx| {
            cx.emit(event.clone())
        });
        let mut this = Self {
            editor,
            multibuffer,
            branch_diff,
            state,
            storage_key,
            buffers: HashMap::default(),
            focus_handle: cx.focus_handle(),
            error: None,
            loading: true,
            _subscriptions: vec![subscription, editor_subscription],
            _refresh_task: Task::ready(()),
            _persist_task: None,
        };
        this.persist(cx);
        this.refresh(window, cx);
        this
    }

    fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.branch_changed(cx) {
            self._refresh_task = Task::ready(());
            self.loading = false;
            cx.notify();
            return;
        }
        self.loading = true;
        cx.notify();
        let paths = self
            .state
            .baselines
            .keys()
            .map(RepoPath::new)
            .collect::<Result<Vec<_>>>();
        let paths = match paths {
            Ok(paths) => paths,
            Err(error) => {
                self.loading = false;
                self.error = Some(error.to_string());
                cx.notify();
                return;
            }
        };
        let entries = self
            .branch_diff
            .update(cx, |diff, cx| diff.load_buffers_including(paths, cx));
        self._refresh_task = cx.spawn_in(window, async move |this, cx| {
            for entry in entries {
                match entry.load.await {
                    Ok((buffer, diff)) => {
                        if this
                            .update_in(cx, |this, window, cx| {
                                this.track_buffer(
                                    entry.repo_path.as_unix_str().to_owned(),
                                    buffer,
                                    diff,
                                    window,
                                    cx,
                                );
                            })
                            .log_err()
                            .is_none()
                        {
                            return;
                        }
                    }
                    Err(error) => {
                        this.update(cx, |this, cx| {
                            this.error = Some(format!(
                                "Could not review {}: {error:#}",
                                entry.repo_path.as_unix_str()
                            ));
                            cx.notify();
                        })
                        .log_err();
                    }
                }
                smol::future::yield_now().await;
            }
            this.update(cx, |this, cx| {
                this.loading = false;
                cx.notify();
            })
            .log_err();
        });
    }

    fn track_buffer(
        &mut self,
        path: String,
        buffer: Entity<Buffer>,
        source_diff: Entity<BufferDiff>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(existing) = self.buffers.get(&path).cloned()
            && existing.read(cx).source == buffer
        {
            self.update_excerpts(&path, &existing, window, cx);
            return;
        }
        let baseline = self.state.baselines.get(&path).cloned().unwrap_or_else(|| {
            source_diff
                .read(cx)
                .base_text_string(cx)
                .unwrap_or_default()
        });
        let review_buffer = cx.new(|cx| ReviewBuffer::new(buffer, baseline, cx));
        let observed_path = path.clone();
        self._subscriptions.push(cx.observe_in(
            &review_buffer,
            window,
            move |this, buffer, window, cx| {
                this.update_excerpts(&observed_path, &buffer, window, cx);
            },
        ));
        self.buffers.insert(path, review_buffer);
    }

    fn update_excerpts(
        &mut self,
        path: &str,
        review: &Entity<ReviewBuffer>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.branch_changed(cx)
            || self.buffers.get(path) != Some(review)
            || review.read(cx).updating
        {
            cx.notify();
            return;
        }
        let review = review.read(cx);
        let snapshot = review.buffer.read(cx).snapshot();
        let diff = review.diff.read(cx).snapshot(cx);
        let ranges = diff
            .hunks(&snapshot)
            .map(|hunk| hunk.buffer_range.to_point(&snapshot))
            .collect::<Vec<_>>();
        let buffer = review.buffer.clone();
        let diff = review.diff.clone();
        let Ok(path) = RepoPath::new(path) else {
            return;
        };
        let path_key = PathKey::with_sort_prefix(0, path.as_ref().clone());
        let was_empty = self.multibuffer.read(cx).is_empty();
        self.multibuffer.update(cx, |multibuffer, cx| {
            multibuffer.update_excerpts_for_path(
                path_key,
                buffer,
                ranges,
                multibuffer_context_lines(cx),
                cx,
            );
            multibuffer.add_diff(diff, cx);
        });
        if was_empty && !self.multibuffer.read(cx).is_empty() {
            let focus_editor = self.focus_handle.is_focused(window);
            self.editor.update(cx, |editor, cx| {
                let snapshot = editor.buffer().read(cx).snapshot(cx);
                if let Some(hunk) = snapshot.diff_hunks().next() {
                    let position = hunk.multi_buffer_range.start;
                    editor.change_selections(Default::default(), window, cx, |selections| {
                        selections.select_ranges([position..position]);
                    });
                }
                if focus_editor {
                    editor.focus_handle(cx).focus(window, cx);
                }
            });
        }
        cx.emit(EditorEvent::TitleChanged);
        cx.notify();
    }

    fn capture_at(
        &self,
        editor: &Entity<Editor>,
        position: editor::Anchor,
        cx: &App,
    ) -> Option<(String, ReviewedHunk)> {
        let editor = editor.read(cx);
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let hunk = editor
            .diff_hunks_in_ranges(&[position..position], &snapshot)
            .next()
            .or_else(|| {
                let (buffer, excerpt) = snapshot.excerpt_containing(position..position)?;
                let row = position.to_point(&snapshot).row;
                // Context lines belong to the nearest change in the same excerpt.
                snapshot
                    .diff_hunks()
                    .filter(|hunk| {
                        hunk.buffer_id == buffer.remote_id() && hunk.excerpt_range == excerpt
                    })
                    .min_by_key(|hunk| {
                        hunk.row_range
                            .start
                            .0
                            .saturating_sub(row)
                            .max(row.saturating_sub(hunk.row_range.end.0.saturating_sub(1)))
                    })
            })?;
        let (path, review) = self
            .buffers
            .iter()
            .find(|(_, review)| review.read(cx).buffer.read(cx).remote_id() == hunk.buffer_id)?;
        let review = review.read(cx);
        let buffer = review.buffer.read(cx).snapshot();
        let diff = review.diff.read(cx).snapshot(cx);
        let hunk = diff
            .hunks_intersecting_range(hunk.buffer_range, &buffer)
            .next()?;
        Some((path.clone(), review.capture(&hunk, cx)?))
    }

    fn mark_selected(&mut self, _: &MarkReviewed, window: &mut Window, cx: &mut Context<Self>) {
        let position = self.editor.read(cx).selections.newest_anchor().head();
        if let Some((path, hunk)) = self.capture_at(&self.editor, position, cx) {
            self.mark_hunk(&path, &hunk, window, cx);
        }
    }

    fn mark_hunk(
        &mut self,
        path: &str,
        hunk: &ReviewedHunk,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.branch_changed(cx) {
            return;
        }
        let Some(review) = self.buffers.get(path) else {
            return;
        };
        match review.update(cx, |review, cx| review.mark_reviewed(hunk, cx)) {
            Ok(()) => {
                self.state
                    .baselines
                    .insert(path.to_owned(), review.read(cx).base_text.to_string());
                self.error = None;
                self.persist(cx);
                self.editor.update(cx, |editor, cx| {
                    let snapshot = editor.buffer().read(cx).snapshot(cx);
                    if let Some(position) = snapshot.anchor_in_excerpt(hunk.position) {
                        editor.change_selections(Default::default(), window, cx, |selections| {
                            selections.select_ranges([position..position]);
                        });
                    }
                    editor.go_to_next_hunk(&editor::actions::GoToHunk, window, cx);
                    editor.focus_handle(cx).focus(window, cx);
                });
            }
            Err(error) => self.error = Some(error.to_string()),
        }
        cx.notify();
    }

    fn persist(&mut self, cx: &mut Context<Self>) {
        let serialized = match serde_json::to_string(&self.state) {
            Ok(serialized) => serialized,
            Err(error) => {
                self.error = Some(format!("Could not save review: {error}"));
                cx.notify();
                return;
            }
        };
        let db = KeyValueStore::global(cx);
        let key = self.storage_key.clone();
        let previous = self._persist_task.take();
        self._persist_task = Some(cx.spawn(async move |this, cx| {
            if let Some(previous) = previous {
                previous.await;
            }
            if let Err(error) = db.write_kvp(key, serialized).await {
                this.update(cx, |this, cx| {
                    this.error = Some(format!("Could not save review: {error}"));
                    cx.notify();
                })
                .log_err();
            }
        }));
    }
}

impl EventEmitter<EditorEvent> for IncrementalReview {}

impl Drop for IncrementalReview {
    fn drop(&mut self) {
        if let Some(task) = self._persist_task.take() {
            task.detach();
        }
    }
}

impl Focusable for IncrementalReview {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        if self.multibuffer.read(cx).is_empty() {
            self.focus_handle.clone()
        } else {
            self.editor.focus_handle(cx)
        }
    }
}

impl Item for IncrementalReview {
    type Event = EditorEvent;
    fn tab_icon(&self, _: &Window, _: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Diff).color(Color::Muted))
    }
    fn tab_content_text(&self, _: usize, _: &App) -> SharedString {
        "Incremental Review".into()
    }
    fn tab_content(&self, params: TabContentParams, _: &Window, _: &App) -> AnyElement {
        Label::new("Incremental Review")
            .color(if params.selected {
                Color::Default
            } else {
                Color::Muted
            })
            .into_any_element()
    }
    fn to_item_events(event: &EditorEvent, f: &mut dyn FnMut(ItemEvent)) {
        Editor::to_item_events(event, f);
    }
    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        handle: &'a Entity<Self>,
        _: &'a App,
    ) -> Option<gpui::AnyEntity> {
        if type_id == TypeId::of::<Self>() {
            Some(handle.clone().into())
        } else if type_id == TypeId::of::<Editor>() {
            Some(self.editor.clone().into())
        } else {
            None
        }
    }
    fn as_searchable(&self, _: &Entity<Self>, _: &App) -> Option<Box<dyn SearchableItemHandle>> {
        Some(Box::new(self.editor.clone()))
    }
    fn breadcrumb_location(&self, _: &App) -> ToolbarItemLocation {
        ToolbarItemLocation::PrimaryLeft
    }
    fn breadcrumbs(&self, cx: &App) -> Option<(Vec<HighlightedText>, Option<Font>)> {
        self.editor.breadcrumbs(cx)
    }
    fn set_nav_history(&mut self, history: ItemNavHistory, _: &mut Window, cx: &mut Context<Self>) {
        self.editor
            .update(cx, |editor, _| editor.set_nav_history(Some(history)));
    }
    fn navigate(
        &mut self,
        data: Arc<dyn Any + Send>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.editor
            .update(cx, |editor, cx| editor.navigate(data, window, cx))
    }
    fn deactivated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editor
            .update(cx, |editor, cx| editor.deactivated(window, cx));
    }
    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, cx| {
            editor.added_to_workspace(workspace, window, cx)
        });
    }
    fn can_save(&self, cx: &App) -> bool {
        self.editor.read(cx).can_save(cx)
    }
    fn is_dirty(&self, cx: &App) -> bool {
        self.multibuffer.read(cx).is_dirty(cx)
    }
    fn has_conflict(&self, cx: &App) -> bool {
        self.multibuffer.read(cx).has_conflict(cx)
    }
    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        self.editor.read(cx).for_each_project_item(cx, f);
    }
    fn reload(
        &mut self,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.editor
            .update(cx, |editor, cx| editor.reload(project, window, cx))
    }
    fn save(
        &mut self,
        options: SaveOptions,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.editor
            .update(cx, |editor, cx| editor.save(options, project, window, cx))
    }
}

impl Render for IncrementalReview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let snapshot = self.multibuffer.read(cx).snapshot(cx);
        let count = snapshot.diff_hunks().count();
        let branch_changed = self.branch_changed(cx);
        let loading = self.loading || self.buffers.values().any(|buffer| buffer.read(cx).updating);
        v_flex()
            .size_full()
            .track_focus(&self.focus_handle)
            .key_context("IncrementalReview")
            .on_action(cx.listener(Self::mark_selected))
            .child(
                h_flex()
                    .px_3()
                    .py_2()
                    .gap_2()
                    .child(Label::new(format!(
                        "{count} unreviewed {}",
                        if count == 1 { "block" } else { "blocks" }
                    )))
                    .child(
                        Button::new("review-selected", "Mark Reviewed")
                            .disabled(count == 0 || branch_changed || loading)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.mark_selected(&MarkReviewed, window, cx)
                            })),
                    ),
            )
            .when_some(self.error.clone(), |view, error| {
                view.child(Label::new(error).color(Color::Error))
            })
            .child(if branch_changed {
                Label::new("Branch changed. Open Incremental Review again to review this branch.")
                    .into_any_element()
            } else if count == 0 {
                v_flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .child(Label::new(if loading {
                        "Loading changes…"
                    } else if self.error.is_some() {
                        "Could not load all changes"
                    } else {
                        "All text changes reviewed"
                    }))
                    .child(
                        Label::new("New edits will appear here automatically.").color(Color::Muted),
                    )
                    .into_any_element()
            } else {
                div()
                    .flex_1()
                    .min_h_0()
                    .child(self.editor.clone())
                    .into_any_element()
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use settings::SettingsStore;

    fn review_buffer(base: &str, current: &str, cx: &mut TestAppContext) -> Entity<ReviewBuffer> {
        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
        });
        let source = cx.new(|cx| Buffer::local(current, cx));
        let review = cx.new(|cx| ReviewBuffer::new(source, base.to_owned(), cx));
        cx.run_until_parked();
        review
    }

    fn hunks(review: &Entity<ReviewBuffer>, cx: &mut TestAppContext) -> Vec<ReviewedHunk> {
        review.read_with(cx, |review, cx| {
            let snapshot = review.buffer.read(cx).snapshot();
            review
                .diff
                .read(cx)
                .snapshot(cx)
                .hunks(&snapshot)
                .map(|hunk| review.capture(&hunk, cx).expect("a current hunk"))
                .collect()
        })
    }

    #[gpui::test]
    fn reviewing_one_block_preserves_other_changes_and_source(cx: &mut TestAppContext) {
        let base = "one\na\nb\nc\nd\ne\nf\ntwo\n";
        let current = "ONE\na\nb\nc\nd\ne\nf\nTWO\n";
        let review = review_buffer(base, current, cx);
        let captured = hunks(&review, cx);
        assert_eq!(captured.len(), 2);
        review
            .update(cx, |review, cx| review.mark_reviewed(&captured[0], cx))
            .expect("review block");
        cx.run_until_parked();
        review.read_with(cx, |review, cx| {
            assert_eq!(review.source.read(cx).text(), current);
            assert_eq!(&*review.base_text, "ONE\na\nb\nc\nd\ne\nf\ntwo\n");
        });
        let remaining = hunks(&review, cx);
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].replacement, "TWO\n");
    }

    #[gpui::test]
    fn edits_to_reviewed_blocks_reappear(cx: &mut TestAppContext) {
        let review = review_buffer("before\n", "after\n", cx);
        let captured = hunks(&review, cx).remove(0);
        review
            .update(cx, |review, cx| review.mark_reviewed(&captured, cx))
            .expect("review block");
        cx.run_until_parked();
        assert!(hunks(&review, cx).is_empty());
        let source = review.read_with(cx, |review, _| review.source.clone());
        source.update(cx, |source, cx| source.set_text("later\n", cx));
        cx.run_until_parked();
        let remaining = hunks(&review, cx);
        assert_eq!(remaining.len(), 1);
        assert_eq!(&*remaining[0].base_text, "after\n");
        assert_eq!(remaining[0].replacement, "later\n");
    }

    #[gpui::test]
    fn stale_click_does_not_acknowledge_new_edits(cx: &mut TestAppContext) {
        let review = review_buffer("before\n", "displayed\n", cx);
        let displayed = hunks(&review, cx).remove(0);
        let source = review.read_with(cx, |review, _| review.source.clone());
        source.update(cx, |source, cx| source.set_text("not yet reviewed\n", cx));
        let result = review.update(cx, |review, cx| review.mark_reviewed(&displayed, cx));
        assert!(result.is_err());
        cx.run_until_parked();
        assert_eq!(
            review
                .read_with(cx, |review, _| review.base_text.clone())
                .as_ref(),
            "before\n"
        );
        assert_eq!(hunks(&review, cx)[0].replacement, "not yet reviewed\n");
    }

    #[gpui::test]
    fn stale_baseline_cannot_acknowledge_a_different_block(cx: &mut TestAppContext) {
        let review = review_buffer(
            "one\na\nb\nc\nd\ne\nf\ntwo\n",
            "LONGER ONE\na\nb\nc\nd\ne\nf\nTWO\n",
            cx,
        );
        let captured = hunks(&review, cx);
        assert_eq!(captured.len(), 2);
        review
            .update(cx, |review, cx| review.mark_reviewed(&captured[0], cx))
            .expect("review first block");
        assert!(
            review
                .update(cx, |review, cx| review.mark_reviewed(&captured[1], cx))
                .is_err()
        );
        cx.run_until_parked();
        let current = hunks(&review, cx).remove(0);
        review
            .update(cx, |review, cx| review.mark_reviewed(&current, cx))
            .expect("review updated block");
        cx.run_until_parked();
        assert!(hunks(&review, cx).is_empty());
    }

    #[gpui::test]
    fn insertions_deletions_and_unicode_can_be_reviewed(cx: &mut TestAppContext) {
        for (base, current) in [("", "é🐢\n"), ("é🐢\n", ""), ("no newline", "different 🐢")]
        {
            let review = review_buffer(base, current, cx);
            let captured = hunks(&review, cx).remove(0);
            review
                .update(cx, |review, cx| review.mark_reviewed(&captured, cx))
                .expect("review block");
            cx.run_until_parked();
            assert!(hunks(&review, cx).is_empty());
            assert_eq!(
                review
                    .read_with(cx, |review, _| review.base_text.clone())
                    .as_ref(),
                current
            );
        }
    }

    #[gpui::test]
    fn saved_baseline_restores_review_progress(cx: &mut TestAppContext) {
        let review = review_buffer("before\n", "reviewed\n", cx);
        let captured = hunks(&review, cx).remove(0);
        review
            .update(cx, |review, cx| review.mark_reviewed(&captured, cx))
            .expect("review block");
        let state = ReviewState {
            base_commit: "baseline-commit".into(),
            baselines: BTreeMap::from_iter([(
                "file.rs".into(),
                review.read_with(cx, |review, _| review.base_text.to_string()),
            )]),
        };
        let serialized = serde_json::to_string(&state).expect("serialize state");
        let restored: ReviewState = serde_json::from_str(&serialized).expect("restore state");
        let baseline = restored.baselines.get("file.rs").expect("saved baseline");
        let restored = review_buffer(baseline, "reviewed\n", cx);
        assert!(hunks(&restored, cx).is_empty());
        let source = restored.read_with(cx, |review, _| review.source.clone());
        source.update(cx, |source, cx| source.set_text("new edit\n", cx));
        cx.run_until_parked();
        assert_eq!(hunks(&restored, cx)[0].replacement, "new edit\n");
    }

    #[gpui::test]
    async fn review_link_selects_repository_and_reuses_tab(cx: &mut TestAppContext) {
        use fs::FakeFs;
        use serde_json::json;
        use std::path::Path;
        use util::path;
        use workspace::MultiWorkspace;

        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            crate::init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        for root in [path!("/first"), path!("/second")] {
            fs.insert_tree(root, json!({".git": {}, "file.txt": "after\n"}))
                .await;
            let git_path = Path::new(root).join(".git");
            fs.set_head_for_repo(&git_path, &[("file.txt", "before\n".into())], "baseline");
            fs.set_index_for_repo(&git_path, &[("file.txt", "before\n".into())]);
            fs.set_merge_base_content_for_repo(&git_path, &[("file.txt", "before\n".into())]);
        }
        let project = Project::test(
            fs,
            [Path::new(path!("/first")), Path::new(path!("/second"))],
            cx,
        )
        .await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |workspace, _| workspace.workspace().clone());
        cx.run_until_parked();
        let first = project.read_with(cx, |project, cx| {
            project
                .repositories(cx)
                .values()
                .find(|repository| {
                    repository.read(cx).work_directory_abs_path.as_ref()
                        == Path::new(path!("/first"))
                })
                .cloned()
                .expect("first repository")
        });
        first.update(cx, |repository, cx| repository.set_as_active_repository(cx));
        for _ in 0..2 {
            workspace
                .update_in(cx, |workspace, window, cx| {
                    open_for_repository(workspace, path!("/second").into(), window, cx)
                })
                .await
                .expect("open linked review");
            workspace.read_with(cx, |workspace, cx| {
                assert_eq!(workspace.items_of_type::<IncrementalReview>(cx).count(), 1);
                let review = workspace
                    .active_item_as::<IncrementalReview>(cx)
                    .expect("active review");
                let repository = review
                    .read(cx)
                    .branch_diff
                    .read(cx)
                    .repo()
                    .expect("review repository");
                assert_eq!(
                    repository.read(cx).work_directory_abs_path.as_ref(),
                    Path::new(path!("/second"))
                );
            });
        }
        let result = workspace
            .update_in(cx, |workspace, window, cx| {
                open_for_repository(workspace, path!("/missing").into(), window, cx)
            })
            .await;
        assert!(result.is_err());
    }

    #[gpui::test]
    async fn mark_reviewed_from_context_selects_nearest_block_in_excerpt(cx: &mut TestAppContext) {
        use fs::FakeFs;
        use language::Point;
        use serde_json::json;
        use std::path::Path;
        use util::path;
        use workspace::MultiWorkspace;

        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            crate::init(cx);
        });
        let lines = (0..40)
            .map(|row| format!("line {row}\n"))
            .collect::<Vec<_>>();
        let base = lines.concat();
        let mut expected = lines.clone();
        let current = lines
            .iter()
            .enumerate()
            .map(|(row, line)| match row {
                5 | 9 | 30 => format!("changed {row}\n"),
                _ => line.clone(),
            })
            .collect::<String>();
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/review-context"),
            json!({".git": {}, "file.txt": current}),
        )
        .await;
        let git_path = Path::new(path!("/review-context/.git"));
        fs.set_head_for_repo(git_path, &[("file.txt", base.clone())], "baseline");
        fs.set_index_for_repo(git_path, &[("file.txt", base.clone())]);
        fs.set_merge_base_content_for_repo(git_path, &[("file.txt", base.clone())]);
        let project = Project::test(fs.clone(), [Path::new(path!("/review-context"))], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |workspace, _| workspace.workspace().clone());
        cx.run_until_parked();
        workspace
            .update_in(cx, |workspace, window, cx| {
                IncrementalReview::open(workspace, window, cx)
            })
            .await
            .expect("open review");
        cx.run_until_parked();
        let review = workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_item_as::<IncrementalReview>(cx)
                .expect("review tab")
        });
        review.read_with(cx, |review, cx| {
            let position = review.editor.read(cx).selections.newest_anchor().head();
            let (_, captured) = review
                .capture_at(&review.editor, position, cx)
                .expect("initial block");
            assert_eq!(captured.replacement, "changed 5\n");
            let snapshot = review.multibuffer.read(cx).snapshot(cx);
            let hunks = snapshot.diff_hunks().collect::<Vec<_>>();
            assert_eq!(hunks.len(), 3);
            assert_eq!(hunks[0].excerpt_range, hunks[1].excerpt_range);
            assert_ne!(hunks[1].excerpt_range, hunks[2].excerpt_range);
        });
        for (remaining, (context_row, changed_row)) in
            [(11, 9), (3, 5), (32, 30)].into_iter().enumerate()
        {
            review.update_in(cx, |review, window, cx| {
                let source = review
                    .buffers
                    .get("file.txt")
                    .expect("tracked file")
                    .read(cx)
                    .source
                    .clone();
                let anchor = source.read(cx).anchor_before(Point::new(context_row, 0));
                let snapshot = review.multibuffer.read(cx).snapshot(cx);
                let position = snapshot
                    .anchor_in_excerpt(anchor)
                    .expect("context in excerpt");
                assert!(
                    review
                        .editor
                        .read(cx)
                        .diff_hunks_in_ranges(&[position..position], &snapshot)
                        .next()
                        .is_none()
                );
                review.editor.update(cx, |editor, cx| {
                    editor.change_selections(Default::default(), window, cx, |selections| {
                        selections.select_ranges([position..position]);
                    });
                });
                review.mark_selected(&MarkReviewed, window, cx);
            });
            cx.run_until_parked();
            expected[changed_row] = format!("changed {changed_row}\n");
            review.read_with(cx, |review, cx| {
                assert_eq!(
                    review.state.baselines.get("file.txt"),
                    Some(&expected.concat())
                );
                assert_eq!(
                    review
                        .multibuffer
                        .read(cx)
                        .snapshot(cx)
                        .diff_hunks()
                        .count(),
                    2 - remaining
                );
                assert!(review.error.is_none());
                assert_eq!(
                    review
                        .buffers
                        .get("file.txt")
                        .expect("tracked file")
                        .read(cx)
                        .source
                        .read(cx)
                        .text(),
                    current
                );
            });
        }
        assert_eq!(
            fs.read_file_sync(path!("/review-context/file.txt"))
                .expect("source file"),
            current.as_bytes()
        );
        assert_eq!(
            fs.with_git_state(git_path, false, |state| state
                .index_contents
                .get(&RepoPath::new("file.txt").expect("file path"))
                .cloned())
                .expect("index")
                .as_deref(),
            Some(base.as_str())
        );
    }

    #[gpui::test]
    async fn external_edits_and_commits_preserve_independent_review_state(cx: &mut TestAppContext) {
        use fs::{FakeFs, Fs, RemoveOptions};
        use serde_json::json;
        use util::path;
        use workspace::MultiWorkspace;

        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            crate::init(cx);
        });
        let base = "one\na\nb\nc\nd\ne\nf\ntwo\n";
        let current = "ONE\na\nb\nc\nd\ne\nf\nTWO\n";
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/review"), json!({".git": {}, "file.txt": current}))
            .await;
        let git_path = std::path::Path::new(path!("/review/.git"));
        fs.set_head_for_repo(git_path, &[("file.txt", base.into())], "baseline");
        fs.set_index_for_repo(git_path, &[("file.txt", base.into())]);
        fs.set_merge_base_content_for_repo(git_path, &[("file.txt", base.into())]);
        fs.set_branch_name(git_path, Some("feature"));

        let project = Project::test(fs.clone(), [std::path::Path::new(path!("/review"))], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |workspace, _| workspace.workspace().clone());
        cx.run_until_parked();
        workspace
            .update_in(cx, |workspace, window, cx| {
                IncrementalReview::open(workspace, window, cx)
            })
            .await
            .expect("open review");
        cx.run_until_parked();
        let review = workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_item_as::<IncrementalReview>(cx)
                .expect("review tab")
        });
        let file = review.read_with(cx, |review, _| {
            review
                .buffers
                .get("file.txt")
                .expect("tracked file")
                .clone()
        });
        let captured = file.read_with(cx, |file, cx| {
            let snapshot = file.buffer.read(cx).snapshot();
            let diff = file.diff.read(cx).snapshot(cx);
            let hunks = diff.hunks(&snapshot).collect::<Vec<_>>();
            assert_eq!(hunks.len(), 2);
            file.capture(&hunks[0], cx).expect("displayed hunk")
        });
        review.update_in(cx, |review, window, cx| {
            review.mark_hunk("file.txt", &captured, window, cx)
        });
        cx.run_until_parked();
        assert_eq!(
            fs.read_file_sync(path!("/review/file.txt"))
                .expect("source file"),
            current.as_bytes()
        );
        let index_text = fs
            .with_git_state(git_path, false, |state| {
                state
                    .index_contents
                    .get(&RepoPath::new("file.txt").expect("path"))
                    .cloned()
            })
            .expect("git index");
        assert_eq!(index_text.as_deref(), Some(base));

        fs.set_head_for_repo(git_path, &[("file.txt", current.into())], "new-commit");
        fs.set_index_for_repo(git_path, &[("file.txt", current.into())]);
        cx.run_until_parked();
        assert_eq!(
            review.read_with(cx, |review, cx| review
                .multibuffer
                .read(cx)
                .snapshot(cx)
                .diff_hunks()
                .count()),
            1
        );

        fs.insert_file(
            path!("/review/file.txt"),
            b"LATER\na\nb\nc\nd\ne\nf\nTWO\n".to_vec(),
        )
        .await;
        cx.run_until_parked();
        assert_eq!(
            review.read_with(cx, |review, cx| review
                .multibuffer
                .read(cx)
                .snapshot(cx)
                .diff_hunks()
                .count()),
            2
        );
        let saved = review.read_with(cx, |review, cx| {
            let serialized = KeyValueStore::global(cx)
                .read_kvp(&review.storage_key)
                .expect("stored state")
                .expect("state exists");
            serde_json::from_str::<ReviewState>(&serialized).expect("deserialize state")
        });
        assert_eq!(saved.base_commit, "baseline");
        assert_eq!(
            saved.baselines.get("file.txt").map(String::as_str),
            Some("ONE\na\nb\nc\nd\ne\nf\ntwo\n")
        );

        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
        pane.update_in(cx, |pane, window, cx| {
            pane.close_active_item(
                &workspace::pane::CloseActiveItem {
                    save_intent: None,
                    close_pinned: false,
                },
                window,
                cx,
            )
        })
        .await
        .expect("close review");
        drop(file);
        drop(review);
        cx.run_until_parked();
        workspace
            .update_in(cx, |workspace, window, cx| {
                IncrementalReview::open(workspace, window, cx)
            })
            .await
            .expect("reopen review");
        cx.run_until_parked();
        let review = workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_item_as::<IncrementalReview>(cx)
                .expect("restored review")
        });
        let file = review.read_with(cx, |review, cx| {
            assert_eq!(review.state.baselines, saved.baselines);
            assert_eq!(
                review
                    .multibuffer
                    .read(cx)
                    .snapshot(cx)
                    .diff_hunks()
                    .count(),
                2
            );
            review
                .buffers
                .get("file.txt")
                .expect("restored file")
                .clone()
        });
        let captured = file.read_with(cx, |file, cx| {
            let snapshot = file.buffer.read(cx).snapshot();
            let diff = file.diff.read(cx).snapshot(cx);
            let hunk = diff.hunks(&snapshot).next().expect("pending hunk");
            file.capture(&hunk, cx).expect("current hunk")
        });
        fs.set_branch_name(git_path, Some("other-branch"));
        cx.run_until_parked();
        review.update_in(cx, |review, window, cx| {
            assert!(review.branch_changed(cx));
            review.mark_hunk("file.txt", &captured, window, cx);
            assert_eq!(review.state.baselines, saved.baselines);
        });
        fs.set_branch_name(git_path, Some("feature"));
        cx.run_until_parked();
        assert!(!review.read_with(cx, |review, cx| review.branch_changed(cx)));

        fs.remove_file(
            std::path::Path::new(path!("/review/file.txt")),
            RemoveOptions::default(),
        )
        .await
        .expect("delete file");
        cx.run_until_parked();
        let deleted = file.read_with(cx, |file, cx| {
            assert!(file.is_deleted(cx));
            let snapshot = file.buffer.read(cx).snapshot();
            let diff = file.diff.read(cx).snapshot(cx);
            let hunk = diff.hunks(&snapshot).next().expect("deleted text");
            file.capture(&hunk, cx).expect("current deletion")
        });
        assert!(deleted.replacement.is_empty());
        review.update_in(cx, |review, window, cx| {
            review.mark_hunk("file.txt", &deleted, window, cx)
        });
        cx.run_until_parked();
        assert_eq!(
            review.read_with(cx, |review, cx| review
                .multibuffer
                .read(cx)
                .snapshot(cx)
                .diff_hunks()
                .count()),
            0
        );
        fs.insert_file(path!("/review/file.txt"), b"recreated\n".to_vec())
            .await;
        cx.run_until_parked();
        assert_eq!(
            review.read_with(cx, |review, cx| review
                .multibuffer
                .read(cx)
                .snapshot(cx)
                .diff_hunks()
                .count()),
            1
        );
    }
}
