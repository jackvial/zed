mod codex;

use anyhow::{Context as _, Result, anyhow, ensure};
use buffer_diff::BufferDiff;
use collections::{BTreeMap, BTreeSet, HashSet};
use db::kvp::KeyValueStore;
use editor::{
    Editor, EditorEvent, MultiBuffer, SelectionEffects, multibuffer_context_lines,
    scroll::Autoscroll,
};
use futures::{StreamExt as _, channel::mpsc};
use git::repository::RepoPath;
use git_ui::review_snapshot::{BranchReviewFile, BranchReviewSnapshot};
use gpui::{
    App, AppContext as _, AsyncApp, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle,
    Focusable, Subscription, Task, WeakEntity, Window, actions,
};
use language::{Buffer, Capability, OffsetRangeExt as _, Point};
use multi_buffer::PathKey;
use project::{Project, ProjectItem as _, ProjectPath, git_store::Repository};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::{ops::Range, sync::Arc, time::Duration};
use terminal::{Terminal, TerminalBuilder};
use terminal_view::TerminalView;
use ui::{Checkbox, ListItem, Tooltip, prelude::*};
use util::{
    ResultExt as _,
    markdown::{MarkdownEscaped, MarkdownInlineCode},
};
use workspace::{
    Item, Workspace, item::ItemEvent, notifications::NotifyTaskExt as _,
    searchable::SearchableItemHandle,
};

actions!(
    review,
    [
        /// Groups all changes on the current branch against dev into a guided review.
        OpenGuidedReview,
        /// Regenerates concept groups from the current branch changes against dev.
        RegenerateGuidedReview,
    ]
);

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ConceptGroup {
    title: String,
    #[serde(default)]
    description: String,
    files: Vec<String>,
}

fn review_markdown(branch: &str, groups: &[ConceptGroup]) -> String {
    let mut markdown = format!(
        "# Guided Review\n\nBranch: {} against `dev`\n",
        MarkdownInlineCode(branch.trim_start_matches("refs/heads/")),
    );
    for (index, group) in groups.iter().enumerate() {
        markdown.push_str(&format!(
            "\n## {}. {}\n\n",
            index + 1,
            MarkdownEscaped(&group.title),
        ));
        if !group.description.is_empty() {
            markdown.push_str(&format!("{}\n\n", MarkdownEscaped(&group.description)));
        }
        markdown.push_str("**Files**\n\n");
        for path in &group.files {
            markdown.push_str(&format!("- {}\n", MarkdownInlineCode(path)));
        }
    }
    markdown
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct SavedReview {
    groups: Vec<ConceptGroup>,
    reviewed: BTreeSet<String>,
    fingerprints: BTreeMap<String, String>,
    generated: bool,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    generated_prompt: Option<String>,
    #[serde(default)]
    model: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReviewFilter {
    All,
    Unreviewed,
    Reviewed,
}

struct ReviewBlock {
    id: String,
    range: Range<Point>,
    base_range: Range<usize>,
    before: String,
    after: String,
}

struct ReviewFile {
    path: RepoPath,
    buffer: Entity<Buffer>,
    diff: Entity<BufferDiff>,
    source_diff: Entity<BufferDiff>,
    blocks: Vec<ReviewBlock>,
    fingerprint: String,
    status: &'static str,
}

impl ReviewFile {
    async fn new(file: &BranchReviewFile, cx: &mut AsyncApp) -> Result<Self> {
        let buffer = cx.new(|cx| {
            let mut buffer = Buffer::local(file.current_text.as_deref().unwrap_or_default(), cx);
            if let Some(source_file) = file.source_snapshot.file() {
                buffer.file_updated(source_file.clone(), cx);
            }
            buffer.set_language(file.source_snapshot.language().cloned(), cx);
            buffer.set_capability(Capability::ReadOnly, cx);
            buffer
        });
        let snapshot = buffer.read_with(cx, |buffer, _| buffer.snapshot());
        let diff = cx.new(|cx| BufferDiff::new(&snapshot.text, cx));
        let update = diff
            .update(cx, |diff, cx| {
                diff.update_diff(
                    snapshot.text.clone(),
                    file.base_text.as_deref().map(Arc::from),
                    Some(true),
                    snapshot.language().cloned(),
                    cx,
                )
            })
            .await;
        diff.update(cx, |diff, cx| diff.set_snapshot(update, &snapshot.text, cx))
            .await;
        let file_fingerprint =
            fingerprint(&(file.path.as_unix_str(), &file.base_text, &file.current_text))?;
        let mut blocks = diff.read_with(cx, |diff, cx| -> Result<Vec<ReviewBlock>> {
            let diff = diff.snapshot(cx);
            let base = diff.base_text().text();
            let current = snapshot.text();
            diff.hunks(&snapshot)
                .map(|hunk| {
                    let before = base
                        .get(hunk.diff_base_byte_range.clone())
                        .context("Invalid base range in review")?
                        .to_owned();
                    let after = current
                        .get(hunk.buffer_range.to_offset(&snapshot))
                        .context("Invalid change range in review")?
                        .to_owned();
                    let id = fingerprint(&(
                        file.path.as_unix_str(),
                        hunk.diff_base_byte_range.start,
                        &before,
                        &after,
                    ))?;
                    Ok(ReviewBlock {
                        id,
                        range: hunk.buffer_range.to_point(&snapshot),
                        base_range: hunk.diff_base_byte_range,
                        before,
                        after,
                    })
                })
                .collect()
        })?;
        if blocks.is_empty() {
            blocks.push(ReviewBlock {
                id: file_fingerprint.clone(),
                range: Point::zero()..Point::zero(),
                base_range: 0..0,
                before: String::new(),
                after: String::new(),
            });
        }
        Ok(Self {
            path: file.path.clone(),
            buffer,
            diff,
            source_diff: file.source_diff.clone(),
            blocks,
            fingerprint: file_fingerprint,
            status: if file.current_text.is_none() {
                "Deleted"
            } else if file.base_text.is_none() {
                "Added"
            } else {
                "Modified"
            },
        })
    }
}

fn fingerprint(value: &impl Serialize) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(value)?)))
}

pub(super) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &OpenGuidedReview, window, cx| {
            if let Err(error) = GuidedReview::open(workspace, window, cx) {
                Task::ready(Err::<(), _>(error)).detach_and_notify_err(
                    workspace.weak_handle(),
                    window,
                    cx,
                );
            }
        });
    })
    .detach();
}

struct GuidedReview {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    repository: Entity<Repository>,
    branch: SharedString,
    snapshot: Option<BranchReviewSnapshot>,
    files: Vec<ReviewFile>,
    saved: SavedReview,
    selected_group: usize,
    selected_file: Option<RepoPath>,
    filter: ReviewFilter,
    multibuffer: Entity<MultiBuffer>,
    editor: Entity<Editor>,
    prompt_editor: Entity<Editor>,
    show_prompt: bool,
    output_terminal: Option<Entity<TerminalView>>,
    focus_handle: FocusHandle,
    loading: bool,
    generating: bool,
    running_model: Option<String>,
    error: Option<String>,
    show_omitted_files: bool,
    last_review_change: Vec<(String, bool)>,
    _subscriptions: Vec<Subscription>,
    _generation_task: Task<()>,
    _persist_task: Option<Task<()>>,
}

impl GuidedReview {
    fn open(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Result<Entity<Self>> {
        let project = workspace.project().clone();
        ensure!(
            project.read(cx).is_local(),
            "Guided review requires a local project"
        );
        let repository = project
            .read(cx)
            .active_repository(cx)
            .context("Open a Git repository to start a guided review")?;
        let existing = workspace
            .items_of_type::<Self>(cx)
            .find(|item| item.read(cx).repository == repository);
        if let Some(existing) = existing {
            if existing.read(cx).branch_changed(cx) {
                existing.update(cx, |review, cx| review.regenerate(false, window, cx));
            }
            workspace.activate_item(&existing, true, true, window, cx);
            return Ok(existing);
        }
        let workspace_handle = workspace.weak_handle();
        let review = cx.new(|cx| Self::new(workspace_handle, project, repository, window, cx));
        workspace.add_item_to_center(Box::new(review.clone()), window, cx);
        review.update(cx, |review, cx| review.regenerate(false, window, cx));
        Ok(review)
    }

    fn new(
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        repository: Entity<Repository>,
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
                Editor::for_multibuffer(multibuffer.clone(), Some(project.clone()), window, cx);
            editor.set_read_only(true);
            editor.set_delegate_open_excerpts(true);
            editor.start_temporary_diff_override();
            editor.disable_diagnostics(cx);
            editor.set_expand_all_diff_hunks(cx);
            editor.set_render_diff_hunk_controls(
                Arc::new(move |row, _, range, _, line_height, editor, _, cx| {
                    let Some(review) = review.upgrade() else {
                        return gpui::Empty.into_any_element();
                    };
                    let Some(id) = review.read(cx).block_at(editor, range, cx) else {
                        return gpui::Empty.into_any_element();
                    };
                    let reviewed = review.read(cx).saved.reviewed.contains(&id);
                    h_flex()
                        .h(line_height)
                        .px_1()
                        .bg(cx.theme().colors().editor_background)
                        .block_mouse_except_scroll()
                        .child(
                            Checkbox::new(("guided-block", row as u64), reviewed.into())
                                .label("Reviewed")
                                .disabled(review.read(cx).stale(cx) || review.read(cx).loading)
                                .on_click(move |checked, window, cx| {
                                    review.update(cx, |review, cx| {
                                        review.set_reviewed(
                                            vec![id.clone()],
                                            checked.selected(),
                                            window,
                                            cx,
                                        )
                                    });
                                }),
                        )
                        .into_any_element()
                }),
                cx,
            );
            editor
        });
        let prompt_editor = cx.new(|cx| {
            let mut editor = Editor::multi_line(window, cx);
            editor.set_text(codex::DEFAULT_PROMPT, window, cx);
            editor.set_show_gutter(false, cx);
            editor.set_show_wrap_guides(false, cx);
            editor.set_soft_wrap_mode(language::language_settings::SoftWrap::EditorWidth, cx);
            editor
        });
        let subscriptions = vec![
            cx.subscribe_in(&editor, window, |this, _, event: &EditorEvent, window, cx| {
                if let EditorEvent::OpenExcerptsRequested {
                    selections_by_buffer,
                    ..
                } = event
                {
                    let location = selections_by_buffer.iter().find_map(|(buffer_id, (ranges, _))| {
                        let file = this.files.iter().find(|file| {
                            file.buffer.read(cx).remote_id() == *buffer_id
                        })?;
                        let buffer = file.buffer.read(cx);
                        Some((
                            buffer.project_path(cx)?,
                            buffer.offset_to_point(ranges.first()?.start.0),
                            file.status == "Deleted",
                            file.source_diff.clone(),
                        ))
                    });
                    if let Some((path, position, deleted, diff)) = location {
                        if deleted {
                            this.error = Some("This file was deleted. Its changes are available in the review.".into());
                            cx.notify();
                        } else {
                            this.open_file_in_split(path, position, diff, window, cx);
                        }
                    }
                } else {
                    cx.emit(event.clone());
                }
            }),
            cx.subscribe(&prompt_editor, |this, editor, event, cx| {
                if matches!(event, EditorEvent::Edited { .. })
                    && this.snapshot.is_some()
                    && !this.loading
                    && !this.branch_changed(cx)
                {
                    let prompt = editor.read(cx).text(cx);
                    if this
                        .saved
                        .prompt
                        .as_deref()
                        .unwrap_or(codex::DEFAULT_PROMPT)
                        != prompt
                    {
                        this.saved.prompt = Some(prompt);
                        this.persist(cx);
                        cx.notify();
                    }
                }
            }),
            cx.subscribe(
                &repository,
                |_, _, _: &project::git_store::RepositoryEvent, cx| cx.notify(),
            ),
            cx.subscribe(&project, |_, _, event, cx| {
                if matches!(
                    event,
                    project::Event::BufferEdited | project::Event::WorktreeUpdatedEntries(..)
                ) {
                    cx.notify();
                }
            }),
        ];
        Self {
            workspace,
            project,
            repository,
            branch: "".into(),
            snapshot: None,
            files: Vec::new(),
            saved: SavedReview::default(),
            selected_group: 0,
            selected_file: None,
            filter: ReviewFilter::Unreviewed,
            multibuffer,
            editor,
            prompt_editor,
            show_prompt: false,
            output_terminal: None,
            focus_handle: cx.focus_handle(),
            loading: false,
            generating: false,
            running_model: None,
            error: None,
            show_omitted_files: false,
            last_review_change: Vec::new(),
            _subscriptions: subscriptions,
            _generation_task: Task::ready(()),
            _persist_task: None,
        }
    }

    fn branch_changed(&self, cx: &App) -> bool {
        self.snapshot.is_some()
            && self
                .repository
                .read(cx)
                .branch
                .as_ref()
                .map(|branch| &branch.ref_name)
                != Some(&self.branch)
    }

    fn stale(&self, cx: &App) -> bool {
        self.branch_changed(cx)
            || self
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| !snapshot.is_current(cx))
    }

    fn storage_key(&self, branch: &str, cx: &App) -> String {
        format!(
            "guided-review:{}:{branch}:dev",
            self.repository.read(cx).work_directory_abs_path.display()
        )
    }

    fn can_export(&self, cx: &App) -> bool {
        self.saved.generated
            && !self.saved.groups.is_empty()
            && !self.loading
            && !self.generating
            && !self.branch_changed(cx)
    }

    fn export_markdown(&self, cx: &mut Context<Self>) -> Task<Result<()>> {
        if !self.can_export(cx) {
            return Task::ready(Err(anyhow!("Generate a guided review before exporting")));
        }
        let markdown = review_markdown(&self.branch, &self.saved.groups);
        let path = cx.prompt_for_new_path(
            &self.repository.read(cx).work_directory_abs_path,
            Some("guided-review.md"),
        );
        let fs = self.project.read(cx).fs().clone();
        cx.spawn(async move |_, _| {
            let Some(path) = path
                .await
                .context("Could not open the export save dialog")??
            else {
                return Ok(());
            };
            fs.atomic_write(path.clone(), markdown)
                .await
                .with_context(|| format!("Could not export guided review to {}", path.display()))
        })
    }

    fn regenerate(&mut self, force: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.snapshot.is_some() && !self.branch_changed(cx) {
            self.saved.prompt = Some(self.prompt_editor.read(cx).text(cx));
        }
        self.loading = true;
        self.generating = false;
        self.running_model = None;
        self.error = None;
        let load =
            BranchReviewSnapshot::load(self.project.clone(), self.repository.clone(), window, cx);
        let environment =
            self.project
                .read(cx)
                .environment()
                .clone()
                .update(cx, |environment, cx| {
                    environment.directory_environment(
                        self.repository.read(cx).work_directory_abs_path.clone(),
                        cx,
                    )
                });
        self._generation_task = cx.spawn_in(window, async move |this, cx| {
            let mut output_terminal = None;
            let result: Result<()> = async {
                let snapshot = load.await?;
                let mut files = Vec::new();
                for file in &snapshot.files {
                    files.push(ReviewFile::new(file, cx).await?);
                }
                let request = this.update_in(
                    cx,
                    |this, window, cx| -> Result<Option<(String, Vec<String>, String)>> {
                        ensure!(
                            snapshot.is_current(cx),
                            "Changes were updated while loading. Regenerate the guided review."
                        );
                        if this.branch != snapshot.branch || this.snapshot.is_none() {
                            let key = this.storage_key(&snapshot.branch, cx);
                            this.saved = KeyValueStore::global(cx)
                                .read_kvp(&key)?
                                .map(|text| serde_json::from_str(&text))
                                .transpose()
                                .context("Could not restore guided review progress")?
                                .unwrap_or_default();
                            this.prompt_editor.update(cx, |editor, cx| {
                                editor.set_text(
                                    this.saved.prompt.as_deref().unwrap_or(codex::DEFAULT_PROMPT),
                                    window,
                                    cx,
                                );
                            });
                        }
                        let instructions = this.prompt_editor.read(cx).text(cx);
                        let fingerprints = files
                            .iter()
                            .map(|file| {
                                (file.path.as_unix_str().to_owned(), file.fingerprint.clone())
                            })
                            .collect();
                        let cached = this.saved.generated
                            && this.saved.fingerprints == fingerprints
                            && this.saved.generated_prompt.as_deref() == Some(instructions.as_str())
                            && this.saved.groups.iter().all(|group| !group.description.is_empty());
                        let ids = files
                            .iter()
                            .flat_map(|file| file.blocks.iter().map(|block| block.id.as_str()))
                            .collect::<HashSet<_>>();
                        this.saved.reviewed.retain(|id| ids.contains(id.as_str()));
                        this.saved.fingerprints = fingerprints;
                        this.saved.generated = cached;
                        this.saved.groups = reconcile_groups(&this.saved.groups, &files);
                        this.branch = snapshot.branch.clone();
                        this.snapshot = Some(snapshot);
                        this.files = files;
                        this.selected_group = this
                            .selected_group
                            .min(this.saved.groups.len().saturating_sub(1));
                        this.last_review_change.clear();
                        this.loading = false;
                        this.show_group(window, cx);
                        let generate = !this.files.is_empty() && (force || !cached);
                        this.generating = generate;
                        this.persist(cx);
                        cx.notify();
                        if generate {
                            let paths = this
                                .files
                                .iter()
                                .map(|file| file.path.as_unix_str().to_owned())
                                .collect();
                            Ok(Some((codex::prompt(&this.files, &instructions)?, paths, instructions)))
                        } else {
                            Ok(None)
                        }
                    },
                )??;
                if let Some((prompt, paths, instructions)) = request {
                    let terminal = Self::open_output_terminal(&this, cx)?;
                    output_terminal = Some(terminal.clone());
                    let environment = environment.await.unwrap_or_default();
                    let (sender, mut receiver) = mpsc::unbounded::<codex::Output>();
                    let generate = cx.background_spawn(async move {
                        codex::generate(prompt, paths, environment, move |output| {
                            sender.unbounded_send(output).log_err();
                        }).await
                    });
                    let timeout = cx.background_executor().timer(Duration::from_secs(180));
                    let generate = async {
                        match futures::future::select(Box::pin(generate), Box::pin(timeout)).await {
                            futures::future::Either::Left((result, _)) => result,
                            futures::future::Either::Right((_, generate)) => {
                                drop(generate);
                                Err(anyhow!("Codex took too long to generate the guide. Regenerate to retry."))
                            }
                        }
                    };
                    let (generated, ()) = futures::join!(generate, async {
                        while let Some(output) = receiver.next().await {
                            match output {
                                codex::Output::Text(text) => terminal.update(cx, |terminal, cx| terminal.write_output(text.as_bytes(), cx)),
                                codex::Output::Model(model) => {
                                    this.update(cx, |this, cx| {
                                        this.running_model = Some(model);
                                        cx.notify();
                                    }).log_err();
                                }
                            }
                        }
                    });
                    let generated = generated?;
                    let refresh = this.update_in(cx, |this, window, cx| {
                        (this.stale(cx) && !this.branch_changed(cx)).then(|| {
                            BranchReviewSnapshot::load(this.project.clone(), this.repository.clone(), window, cx)
                        })
                    })?;
                    let refreshed = if let Some(refresh) = refresh {
                        match refresh.await {
                            Ok(snapshot) => Some(snapshot),
                            Err(error) => {
                                log::warn!("Could not refresh guided review snapshot: {error:#}");
                                None
                            }
                        }
                    } else {
                        None
                    };
                    this.update_in(cx, |this, window, cx| -> Result<()> {
                        ensure!(
                            !this.branch_changed(cx),
                            "Branch changed while Codex was grouping changes. Regenerate the guided review."
                        );
                        if let Some(refreshed) = refreshed {
                            this.refresh_snapshot(refreshed);
                        }
                        this.saved.groups = generated.groups;
                        this.saved.model = Some(generated.model);
                        this.saved.generated = true;
                        this.saved.generated_prompt = Some(instructions);
                        this.selected_group = 0;
                        this.show_group(window, cx);
                        this.persist(cx);
                        if this.stale(cx) {
                            terminal.update(cx, |terminal, cx| terminal.write_output(
                                b"\nThe branch changed during generation. The guide is available for the captured snapshot; regenerate to include the latest changes.\n", cx
                            ));
                        }
                        Ok(())
                    })??;
                }
                Ok(())
            }
            .await;
            if let Some(terminal) = output_terminal {
                let message = match &result {
                    Ok(()) => "\nGuided review ready. Return to the Guided Review tab.\n".to_owned(),
                    Err(error) => format!("\nGuided review failed: {error:#}\n"),
                };
                terminal.update(cx, |terminal, cx| terminal.write_output(message.as_bytes(), cx));
            }
            this.update(cx, |this, cx| {
                this.loading = false;
                this.generating = false;
                if let Err(error) = result {
                    this.error = Some(format!("{error:#}"));
                }
                cx.notify();
            })
            .log_err();
        });
        cx.notify();
    }

    fn refresh_snapshot(&mut self, snapshot: BranchReviewSnapshot) {
        if self.snapshot.as_ref().is_some_and(|previous| {
            previous.branch == snapshot.branch
                && previous.files.len() == snapshot.files.len()
                && previous
                    .files
                    .iter()
                    .zip(&snapshot.files)
                    .all(|(before, after)| {
                        before.path == after.path
                            && before.base_text == after.base_text
                            && before.current_text == after.current_text
                    })
        }) {
            self.snapshot = Some(snapshot);
        }
    }

    fn open_output_terminal(
        this: &WeakEntity<Self>,
        cx: &mut AsyncWindowContext,
    ) -> Result<Entity<Terminal>> {
        let (terminal, view, workspace) =
            this.update_in(cx, |this, window, cx| -> Result<_> {
                let builder = TerminalBuilder::new_display_only(
                    Default::default(),
                    settings::AlternateScroll::On,
                    None,
                    window.window_handle().window_id().as_u64(),
                    cx.background_executor(),
                    util::paths::PathStyle::local(),
                )?;
                let terminal = cx.new(|cx| builder.subscribe(cx));
                terminal.update(cx, |terminal, cx| {
                    terminal.write_output(
                        format!(
                            "Guided review · {} against dev\n\n",
                            this.branch.trim_start_matches("refs/heads/")
                        )
                        .as_bytes(),
                        cx,
                    );
                });
                let view = cx.new(|cx| {
                    let mut view = TerminalView::new(
                        terminal.clone(),
                        this.workspace.clone(),
                        None,
                        this.project.downgrade(),
                        window,
                        cx,
                    );
                    view.set_custom_title(Some("Codex Guided Review".into()), cx);
                    view
                });
                this.output_terminal = Some(view.clone());
                Ok((terminal, view, this.workspace.clone()))
            })??;
        let opened = workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_center(Box::new(view.clone()), window, cx)
        })?;
        ensure!(opened, "Could not open the Codex output terminal");
        Ok(terminal)
    }

    fn block_at(
        &self,
        editor: &Entity<Editor>,
        range: Range<editor::Anchor>,
        cx: &App,
    ) -> Option<String> {
        let editor = editor.read(cx);
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let hunk = editor.diff_hunks_in_ranges(&[range], &snapshot).next()?;
        let file = self
            .files
            .iter()
            .find(|file| file.buffer.read(cx).remote_id() == hunk.buffer_id)?;
        file.blocks
            .iter()
            .find(|block| {
                block.base_range
                    == (hunk.diff_base_byte_range.start.0..hunk.diff_base_byte_range.end.0)
            })
            .map(|block| block.id.clone())
    }

    fn visible(&self, block: &ReviewBlock) -> bool {
        match self.filter {
            ReviewFilter::All => true,
            ReviewFilter::Reviewed => self.saved.reviewed.contains(&block.id),
            ReviewFilter::Unreviewed => !self.saved.reviewed.contains(&block.id),
        }
    }

    fn group_blocks(&self, index: usize) -> Vec<String> {
        let Some(group) = self.saved.groups.get(index) else {
            return Vec::new();
        };
        self.files
            .iter()
            .filter(|file| {
                group
                    .files
                    .iter()
                    .any(|path| path == file.path.as_unix_str())
            })
            .flat_map(|file| file.blocks.iter().map(|block| block.id.clone()))
            .collect()
    }

    fn checked(&self, ids: &[String]) -> ToggleState {
        ToggleState::from_any_and_all(
            ids.iter().any(|id| self.saved.reviewed.contains(id)),
            !ids.is_empty() && ids.iter().all(|id| self.saved.reviewed.contains(id)),
        )
    }

    fn show_group(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.multibuffer.update(cx, |buffer, cx| buffer.clear(cx));
        self.selected_file = None;
        if let Some(group) = self.saved.groups.get(self.selected_group) {
            for (index, path) in group.files.iter().enumerate() {
                if let Some(file) = self
                    .files
                    .iter()
                    .find(|file| file.path.as_unix_str() == path)
                {
                    let ranges = file
                        .blocks
                        .iter()
                        .filter(|block| self.visible(block))
                        .map(|block| block.range.clone())
                        .collect::<Vec<_>>();
                    if ranges.is_empty() {
                        continue;
                    }
                    if self.selected_file.is_none() {
                        self.selected_file = Some(file.path.clone());
                    }
                    self.multibuffer.update(cx, |buffer, cx| {
                        buffer.update_excerpts_for_path(
                            PathKey::with_sort_prefix(index as u64, file.path.as_ref().clone()),
                            file.buffer.clone(),
                            ranges,
                            multibuffer_context_lines(cx),
                            cx,
                        );
                        buffer.add_diff(file.diff.clone(), cx);
                    });
                }
            }
        }
        if let Some(path) = self.selected_file.clone() {
            self.select_file(&path, window, cx);
        }
        cx.notify();
    }

    fn select_file(&mut self, path: &RepoPath, window: &mut Window, cx: &mut Context<Self>) {
        let Some(file) = self.files.iter().find(|file| &file.path == path) else {
            return;
        };
        let Some(block) = file.blocks.iter().find(|block| self.visible(block)) else {
            return;
        };
        let anchor = file.buffer.read(cx).anchor_before(block.range.start);
        let position = self
            .multibuffer
            .read(cx)
            .snapshot(cx)
            .anchor_in_excerpt(anchor);
        if let Some(position) = position {
            self.selected_file = Some(path.clone());
            self.editor.update(cx, |editor, cx| {
                editor.change_selections(
                    SelectionEffects::scroll(Autoscroll::center()),
                    window,
                    cx,
                    |selections| selections.select_ranges([position..position]),
                )
            });
            cx.notify();
        }
    }

    fn open_file_in_split(
        &self,
        path: ProjectPath,
        position: Point,
        diff: Entity<BufferDiff>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result: Result<()> = async {
                let item = workspace
                    .update_in(cx, |workspace, window, cx| {
                        let pane = workspace.adjacent_pane(window, cx);
                        workspace.open_path(path, Some(pane.downgrade()), true, window, cx)
                    })?
                    .await?;
                if let Some(editor) = item.downcast::<Editor>() {
                    editor.update_in(cx, |editor, window, cx| -> Result<()> {
                        editor.start_temporary_diff_override();
                        editor
                            .buffer()
                            .update(cx, |buffer, cx| buffer.add_diff(diff, cx));
                        editor.set_expand_all_diff_hunks(cx);
                        editor.set_render_diff_hunk_controls(
                            Arc::new(|_, _, _, _, _, _, _, _| gpui::Empty.into_any_element()),
                            cx,
                        );
                        let buffer = editor.buffer().read(cx);
                        let source = buffer
                            .as_singleton()
                            .context("Expected a single file editor")?;
                        let source = source.read(cx);
                        let position = source.clip_point(position, language::Bias::Left);
                        let anchor = source.anchor_before(position);
                        let position = buffer
                            .snapshot(cx)
                            .anchor_in_excerpt(anchor)
                            .context("Could not locate the file position in its diff")?;
                        editor.change_selections(
                            SelectionEffects::scroll(Autoscroll::center()),
                            window,
                            cx,
                            |selections| selections.select_ranges([position..position]),
                        );
                        Ok(())
                    })??;
                }
                Ok(())
            }
            .await;
            if let Err(error) = result {
                this.update(cx, |this, cx| {
                    this.error = Some(format!("Could not open the file in a split: {error:#}"));
                    cx.notify();
                })
                .log_err();
            }
        })
        .detach();
    }

    fn set_reviewed(
        &mut self,
        ids: Vec<String>,
        reviewed: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.stale(cx) || self.loading {
            return;
        }
        self.last_review_change = ids
            .into_iter()
            .filter_map(|id| {
                let previous = self.saved.reviewed.contains(&id);
                if previous == reviewed {
                    return None;
                }
                if reviewed {
                    self.saved.reviewed.insert(id.clone());
                } else {
                    self.saved.reviewed.remove(&id);
                }
                Some((id, previous))
            })
            .collect();
        self.persist(cx);
        self.show_group(window, cx);
    }

    fn undo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.stale(cx) || self.loading {
            return;
        }
        for (id, previous) in std::mem::take(&mut self.last_review_change) {
            if previous {
                self.saved.reviewed.insert(id);
            } else {
                self.saved.reviewed.remove(&id);
            }
        }
        self.persist(cx);
        self.show_group(window, cx);
    }

    fn persist(&mut self, cx: &mut Context<Self>) {
        let serialized = match serde_json::to_string(&self.saved) {
            Ok(serialized) => serialized,
            Err(error) => {
                self.error = Some(format!("Could not save guided review: {error}"));
                cx.notify();
                return;
            }
        };
        let key = self.storage_key(&self.branch, cx);
        let database = KeyValueStore::global(cx);
        let previous = self._persist_task.take();
        self._persist_task = Some(cx.spawn(async move |this, cx| {
            if let Some(previous) = previous {
                previous.await;
            }
            if let Err(error) = database.write_kvp(key, serialized).await {
                this.update(cx, |this, cx| {
                    this.error = Some(format!("Could not save guided review: {error}"));
                    cx.notify();
                })
                .log_err();
            }
        }));
    }
}

fn reconcile_groups(groups: &[ConceptGroup], files: &[ReviewFile]) -> Vec<ConceptGroup> {
    let mut remaining = files
        .iter()
        .map(|file| file.path.as_unix_str())
        .collect::<HashSet<_>>();
    let mut groups = groups
        .iter()
        .filter_map(|group| {
            let files = group
                .files
                .iter()
                .filter(|path| remaining.remove(path.as_str()))
                .cloned()
                .collect::<Vec<_>>();
            (!files.is_empty()).then(|| ConceptGroup {
                title: group.title.clone(),
                description: group.description.clone(),
                files,
            })
        })
        .collect::<Vec<_>>();
    let missing = files
        .iter()
        .filter(|file| remaining.contains(file.path.as_unix_str()))
        .map(|file| file.path.as_unix_str().to_owned())
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        groups.push(ConceptGroup {
            title: "Ungrouped changes".into(),
            description: String::new(),
            files: missing,
        });
    }
    groups
}

impl Drop for GuidedReview {
    fn drop(&mut self) {
        if let Some(task) = self._persist_task.take() {
            task.detach();
        }
    }
}

impl EventEmitter<EditorEvent> for GuidedReview {}

impl Focusable for GuidedReview {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        if self.multibuffer.read(cx).is_empty() || self.branch_changed(cx) {
            self.focus_handle.clone()
        } else {
            self.editor.focus_handle(cx)
        }
    }
}

impl Item for GuidedReview {
    type Event = EditorEvent;
    fn tab_content_text(&self, _: usize, _: &App) -> SharedString {
        "Guided Review".into()
    }
    fn tab_icon(&self, _: &Window, _: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Diff).color(Color::Muted))
    }
    fn to_item_events(event: &EditorEvent, emit: &mut dyn FnMut(ItemEvent)) {
        Editor::to_item_events(event, emit);
    }
    fn show_toolbar(&self) -> bool {
        false
    }
    fn as_searchable(&self, _: &Entity<Self>, _: &App) -> Option<Box<dyn SearchableItemHandle>> {
        Some(Box::new(self.editor.clone()))
    }
}

impl Render for GuidedReview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let branch_changed = self.branch_changed(cx);
        let stale = self.stale(cx);
        let prompt_read_only =
            self.snapshot.is_none() || branch_changed || self.loading || self.generating;
        self.prompt_editor
            .update(cx, |editor, _| editor.set_read_only(prompt_read_only));
        let prompt_changed = self
            .saved
            .generated_prompt
            .as_deref()
            .is_some_and(|generated| {
                generated
                    != self
                        .saved
                        .prompt
                        .as_deref()
                        .unwrap_or(codex::DEFAULT_PROMPT)
            });
        let omitted_files = self
            .snapshot
            .as_ref()
            .filter(|_| !branch_changed)
            .map(|snapshot| snapshot.omitted_files.as_slice())
            .unwrap_or_default();
        let total = self
            .files
            .iter()
            .map(|file| file.blocks.len())
            .sum::<usize>();
        let reviewed = self.saved.reviewed.len();
        let branch = self
            .repository
            .read(cx)
            .branch
            .as_ref()
            .map(|branch| branch.name().to_owned())
            .unwrap_or_else(|| "Detached HEAD".into());
        let group = self.saved.groups.get(self.selected_group);
        let group_ids = self.group_blocks(self.selected_group);
        let group_state = self.checked(&group_ids);
        let file_rows = group
            .into_iter()
            .flat_map(|group| group.files.iter())
            .filter_map(|path| {
                let file = self
                    .files
                    .iter()
                    .find(|file| file.path.as_unix_str() == path)?;
                let path = file.path.clone();
                let ids = file
                    .blocks
                    .iter()
                    .map(|block| block.id.clone())
                    .collect::<Vec<_>>();
                let checked = self.checked(&ids);
                let visible = file.blocks.iter().any(|block| self.visible(block));
                Some(
                    ListItem::new(SharedString::from(path.as_unix_str().to_owned()))
                        .inset(true)
                        .toggle_state(self.selected_file.as_ref() == Some(&path))
                        .child(
                            v_flex()
                                .min_w_0()
                                .gap_0p5()
                                .child(Label::new(path.as_unix_str().to_owned()))
                                .child(
                                    Label::new(format!(
                                        "{} · {} blocks",
                                        file.status,
                                        file.blocks.len()
                                    ))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                                ),
                        )
                        .end_slot(
                            Checkbox::new(
                                SharedString::from(format!("guided-file:{}", path.as_unix_str())),
                                checked,
                            )
                            .disabled(stale || self.loading)
                            .on_click(cx.listener(
                                move |this, checked: &ToggleState, window, cx| {
                                    cx.stop_propagation();
                                    this.set_reviewed(ids.clone(), checked.selected(), window, cx);
                                },
                            )),
                        )
                        .on_click(cx.listener(move |this, _, window, cx| {
                            if visible {
                                this.select_file(&path, window, cx);
                                window.focus(&this.editor.focus_handle(cx), cx);
                            }
                        })),
                )
            })
            .collect::<Vec<_>>();
        let rail = self
            .saved
            .groups
            .iter()
            .enumerate()
            .map(|(index, _)| {
                let reviewed = self.checked(&self.group_blocks(index)) == ToggleState::Selected;
                Button::new(
                    ("guided-group", index),
                    if reviewed {
                        "✓".to_owned()
                    } else {
                        format!("{:02}", index + 1)
                    },
                )
                .toggle_state(index == self.selected_group)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.selected_group = index;
                    this.show_group(window, cx);
                }))
            })
            .collect::<Vec<_>>();
        let filters = [
            (
                ReviewFilter::Unreviewed,
                format!(
                    "Unreviewed {}",
                    if branch_changed {
                        0
                    } else {
                        total.saturating_sub(reviewed)
                    }
                ),
            ),
            (
                ReviewFilter::Reviewed,
                format!("Reviewed {}", if branch_changed { 0 } else { reviewed }),
            ),
            (ReviewFilter::All, "All".into()),
        ];
        let header =
            h_flex()
                .px_3()
                .py_2()
                .gap_2()
                .justify_between()
                .border_b_1()
                .border_color(cx.theme().colors().border)
                .child(
                    v_flex()
                        .child(
                            Label::new(format!("{branch} against dev"))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .when(!branch_changed, |view| {
                            view.child(
                                Label::new(if self.generating {
                                    self.running_model
                                        .as_ref()
                                        .map(|model| format!("Model: {model}"))
                                        .unwrap_or_else(|| "Model: starting Codex…".into())
                                } else {
                                    self.saved
                                        .model
                                        .as_ref()
                                        .map(|model| format!("Model: {model}"))
                                        .unwrap_or_else(|| {
                                            "Model not recorded · regenerate to show model".into()
                                        })
                                })
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                            )
                        }),
                )
                .child(
                    h_flex()
                        .gap_1()
                        .children(filters.into_iter().enumerate().map(
                            |(index, (filter, label))| {
                                Button::new(("guided-filter", index), label)
                                    .toggle_state(self.filter == filter)
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.filter = filter;
                                        this.show_group(window, cx);
                                    }))
                            },
                        )),
                );
        let body = if branch_changed || self.snapshot.is_none() || self.files.is_empty() {
            let message = if self.loading {
                "Loading branch changes…"
            } else if self.error.is_some() {
                "Could not load guided review"
            } else if branch_changed {
                "Generate a guide for this branch"
            } else if !omitted_files.is_empty() {
                "No files could be loaded for review. See omitted files above."
            } else {
                "No changes against dev"
            };
            v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .child(Label::new(message).color(Color::Muted))
                .into_any_element()
        } else {
            let navigation = h_flex()
                .gap_1()
                .child(
                    Button::new("guided-previous", "Previous")
                        .disabled(self.selected_group == 0)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.selected_group = this.selected_group.saturating_sub(1);
                            this.show_group(window, cx);
                        })),
                )
                .child(
                    Button::new("guided-next", "Next")
                        .disabled(self.selected_group + 1 >= self.saved.groups.len())
                        .on_click(cx.listener(|this, _, window, cx| {
                            if this.selected_group + 1 < this.saved.groups.len() {
                                this.selected_group += 1;
                                this.show_group(window, cx);
                            }
                        })),
                );
            let sidebar = h_flex()
                .w(relative(0.27))
                .min_w(px(280.))
                .max_w(px(480.))
                .h_full()
                .items_start()
                .p_3()
                .gap_2()
                .border_r_1()
                .border_color(cx.theme().colors().border)
                .child(
                    v_flex()
                        .id("guided-concepts")
                        .h_full()
                        .overflow_y_scroll()
                        .gap_1()
                        .children(rail),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .gap_4()
                        .child(
                            Label::new(if self.generating {
                                "Grouping with Codex…"
                            } else {
                                "Concepts"
                            })
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                        )
                        .child(
                            Label::new(group.map(|group| group.title.clone()).unwrap_or_default())
                                .size(LabelSize::Large),
                        )
                        .child(
                            h_flex()
                                .gap_2()
                                .child(
                                    Label::new(format!(
                                        "{} / {}",
                                        self.selected_group + 1,
                                        self.saved.groups.len()
                                    ))
                                    .size(LabelSize::Small),
                                )
                                .child(
                                    Checkbox::new("guided-group-reviewed", group_state)
                                        .label("Reviewed")
                                        .disabled(stale || self.loading)
                                        .on_click(cx.listener(
                                            move |this, checked: &ToggleState, window, cx| {
                                                this.set_reviewed(
                                                    group_ids.clone(),
                                                    checked.selected(),
                                                    window,
                                                    cx,
                                                );
                                            },
                                        )),
                                ),
                        )
                        .child(
                            v_flex()
                                .id("guided-files")
                                .flex_1()
                                .min_h_0()
                                .overflow_y_scroll()
                                .when_some(
                                    group.filter(|group| !group.description.is_empty()),
                                    |view, group| {
                                        view.child(
                                            div()
                                                .pb_4()
                                                .child(Label::new(group.description.clone())),
                                        )
                                    },
                                )
                                .children(file_rows),
                        )
                        .child(navigation),
                );
            let diff = if self.multibuffer.read(cx).is_empty() {
                v_flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .child(
                        Label::new("No blocks match this filter in this group").color(Color::Muted),
                    )
                    .into_any_element()
            } else {
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .child(self.editor.clone())
                    .into_any_element()
            };
            h_flex()
                .flex_1()
                .min_h_0()
                .items_stretch()
                .child(sidebar)
                .child(diff)
                .into_any_element()
        };
        let footer = h_flex()
            .flex_shrink_0()
            .px_3()
            .py_2()
            .justify_between()
            .border_t_1()
            .border_color(cx.theme().colors().border)
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Label::new(if branch_changed {
                            "Local branch review".to_owned()
                        } else {
                            format!("{reviewed} / {total} blocks reviewed")
                        })
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    )
                    .when(!self.last_review_change.is_empty(), |view| {
                        view.child(
                            Button::new("guided-undo", "Undo")
                                .disabled(stale || self.loading)
                                .on_click(cx.listener(|this, _, window, cx| this.undo(window, cx))),
                        )
                    }),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("guided-export-markdown", "Export Markdown")
                            .disabled(!self.can_export(cx))
                            .tooltip(Tooltip::text(
                                "Export all concept titles, descriptions, and file paths",
                            ))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.export_markdown(cx).detach_and_notify_err(
                                    this.workspace.clone(),
                                    window,
                                    cx,
                                );
                            })),
                    )
                    .child(
                        Button::new(
                            "guided-review-prompt",
                            if prompt_changed {
                                "Review prompt · changed"
                            } else {
                                "Review prompt"
                            },
                        )
                        .toggle_state(self.show_prompt)
                        .disabled(branch_changed)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.show_prompt = !this.show_prompt;
                            let focus = if this.show_prompt {
                                this.prompt_editor.focus_handle(cx)
                            } else {
                                this.focus_handle(cx)
                            };
                            window.focus(&focus, cx);
                            cx.notify();
                        })),
                    )
                    .when_some(self.output_terminal.clone(), |view, terminal| {
                        view.child(Button::new("guided-codex-output", "Codex output").on_click(
                            cx.listener(move |this, _, window, cx| {
                                let workspace = this.workspace.clone();
                                let terminal = terminal.clone();
                                window.defer(cx, move |window, cx| {
                                    workspace
                                        .update(cx, |workspace, cx| {
                                            if !workspace
                                                .activate_item(&terminal, true, true, window, cx)
                                            {
                                                workspace.add_item_to_center(
                                                    Box::new(terminal.clone()),
                                                    window,
                                                    cx,
                                                );
                                            }
                                        })
                                        .log_err();
                                });
                            }),
                        ))
                    })
                    .when(self.loading || self.generating, |view| {
                        view.child(Button::new("guided-cancel", "Cancel").on_click(cx.listener(
                            |this, _, _, cx| {
                                if this.generating
                                    && let Some(view) = &this.output_terminal
                                {
                                    view.read(cx).terminal().clone().update(cx, |terminal, cx| {
                                        terminal.write_output(
                                            b"\nGuided review generation cancelled.\n",
                                            cx,
                                        );
                                    });
                                }
                                this._generation_task = Task::ready(());
                                this.loading = false;
                                this.generating = false;
                                cx.notify();
                            },
                        )))
                    })
                    .child(
                        Button::new("regenerate-guided-review", "Regenerate Guided Review")
                            .disabled(self.loading || self.generating)
                            .tooltip(Tooltip::text(
                                "Regroup all current branch changes against dev with Codex",
                            ))
                            .on_click(
                                cx.listener(|this, _, window, cx| {
                                    this.regenerate(true, window, cx)
                                }),
                            ),
                    ),
            );
        v_flex()
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .track_focus(&self.focus_handle)
            .key_context("GuidedReview")
            .on_action(cx.listener(|this, _: &RegenerateGuidedReview, window, cx| {
                this.regenerate(true, window, cx);
            }))
            .child(header)
            .when(!omitted_files.is_empty(), |view| {
                view.child(
                    v_flex()
                        .px_3()
                        .py_2()
                        .gap_1()
                        .child(
                            Button::new(
                                "guided-omitted-files",
                                format!("Files omitted from this review: {}", omitted_files.len()),
                            )
                            .color(Color::Warning)
                            .start_icon(Icon::new(if self.show_omitted_files {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronRight
                            }))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.show_omitted_files = !this.show_omitted_files;
                                cx.notify();
                            })),
                        )
                        .when(self.show_omitted_files, |view| {
                            view.child(
                                v_flex()
                                    .id("guided-omitted-file-list")
                                    .max_h(px(140.))
                                    .overflow_y_scroll()
                                    .children(omitted_files.iter().map(|(path, reason)| {
                                        Label::new(format!("{}: {reason}", path.as_unix_str()))
                                            .size(LabelSize::Small)
                                            .color(Color::Muted)
                                    })),
                            )
                        }),
                )
            })
            .when_some(self.error.clone(), |view, error| {
                view.child(
                    div()
                        .px_3()
                        .py_2()
                        .child(Label::new(error).color(Color::Error).size(LabelSize::Small)),
                )
            })
            .when(stale && !self.loading, |view| {
                let message = if branch_changed {
                    "Branch changed. Regenerate to review the current branch."
                } else {
                    "Changes have been updated. Regenerate before checking off more blocks."
                };
                view.child(
                    div().px_3().py_2().child(
                        Label::new(message)
                            .color(Color::Warning)
                            .size(LabelSize::Small),
                    ),
                )
            })
            .child(body)
            .when(self.show_prompt && !branch_changed, |view| {
                view.child(
                    v_flex()
                        .flex_shrink_0()
                        .px_3()
                        .py_2()
                        .gap_2()
                        .border_t_1()
                        .border_color(cx.theme().colors().border)
                        .child(
                            Label::new(if self.generating {
                                "Generating from this prompt. Current branch changes are attached automatically."
                            } else if prompt_changed {
                                "Prompt changed. Regenerate to apply. Current branch changes are attached automatically."
                            } else {
                                "Edit this prompt, then regenerate. Current branch changes are attached automatically."
                            })
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                        )
                        .child(
                            div()
                                .h(rems(12.))
                                .p_2()
                                .border_1()
                                .border_color(cx.theme().colors().border)
                                .rounded_md()
                                .child(self.prompt_editor.clone()),
                        ),
                )
            })
            .child(footer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::{FakeFs, Fs as _};
    use gpui::TestAppContext;
    use serde_json::json;
    use settings::SettingsStore;
    use std::path::Path;
    use util::path;
    use workspace::MultiWorkspace;

    #[gpui::test]
    async fn review_controls_persist_and_remain_branch_scoped(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            git_ui::init(cx);
            init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        let base = "one\na\nb\nc\nd\ne\nf\ng\nh\ni\ntwo\n";
        let current = "ONE\na\nb\nc\nd\ne\nf\ng\nh\ni\nTWO\n";
        fs.insert_tree(
            path!("/guided-controls"),
            json!({
                ".git": {}, "model.txt": current, "test.txt": "new test\n",
                "document.pdf": "%PDF-1.7\nbinary fixture\n"
            }),
        )
        .await;
        let git_path = Path::new(path!("/guided-controls/.git"));
        fs.set_branch_name(git_path, Some("feature"));
        fs.set_head_for_repo(
            git_path,
            &[
                ("model.txt", current.into()),
                ("test.txt", "new test\n".into()),
            ],
            "feature-head",
        );
        fs.set_index_for_repo(
            git_path,
            &[
                ("model.txt", current.into()),
                ("test.txt", "new test\n".into()),
            ],
        );
        fs.set_merge_base_content_for_repo(git_path, &[("model.txt", base.into())]);
        let project = Project::test(fs.clone(), [Path::new(path!("/guided-controls"))], cx).await;
        let repository = project.read_with(cx, |project, cx| {
            project.active_repository(cx).expect("repository")
        });
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |workspace, _| workspace.workspace().clone());
        cx.run_until_parked();
        let snapshot = cx
            .update(|window, cx| {
                BranchReviewSnapshot::load(project.clone(), repository.clone(), window, cx)
            })
            .await
            .expect("snapshot");
        let mut files = Vec::new();
        for file in &snapshot.files {
            files.push(
                ReviewFile::new(file, &mut cx.to_async())
                    .await
                    .expect("review file"),
            );
        }
        let saved = SavedReview {
            groups: vec![
                ConceptGroup {
                    title: "Update model".into(),
                    description: "Changes two model behaviors.".into(),
                    files: vec!["model.txt".into()],
                },
                ConceptGroup {
                    title: "Test model".into(),
                    description: "Adds coverage for the model.".into(),
                    files: vec!["test.txt".into()],
                },
            ],
            fingerprints: files
                .iter()
                .map(|file| (file.path.as_unix_str().to_owned(), file.fingerprint.clone()))
                .collect(),
            generated: true,
            prompt: Some("Explain the model changes and their tests.".into()),
            generated_prompt: Some("Explain the model changes and their tests.".into()),
            model: Some("test-model".into()),
            ..Default::default()
        };
        let database = cx.read(KeyValueStore::global);
        for branch in ["feature", "another"] {
            let mut saved = saved.clone();
            if branch == "another" {
                saved.prompt = Some("Focus on tests for this branch.".into());
                saved.generated_prompt = saved.prompt.clone();
            }
            database
                .write_kvp(
                    format!(
                        "guided-review:{}:refs/heads/{branch}:dev",
                        path!("/guided-controls")
                    ),
                    serde_json::to_string(&saved).expect("serialize seed"),
                )
                .await
                .expect("seed guide");
        }
        let review = workspace
            .update_in(cx, |workspace, window, cx| {
                GuidedReview::open(workspace, window, cx)
            })
            .expect("open guide");
        review.read_with(cx, |review, cx| {
            assert!(review.snapshot.is_none());
            assert!(!review.branch_changed(cx));
        });
        cx.run_until_parked();
        review.read_with(cx, |review, cx| {
            assert!(review.error.is_none(), "{:?}", review.error);
            assert!(!review.loading && !review.generating);
            assert_eq!(
                review
                    .snapshot
                    .as_ref()
                    .expect("loaded snapshot")
                    .omitted_files
                    .len(),
                1
            );
            assert_eq!(review.saved.groups.len(), 2);
            assert_eq!(
                review.saved.groups[0].description,
                "Changes two model behaviors."
            );
            assert_eq!(
                review.prompt_editor.read(cx).text(cx),
                "Explain the model changes and their tests."
            );
            assert_eq!(review.group_blocks(0).len(), 2);
            assert!(review.editor.read(cx).read_only(cx));
            assert_eq!(
                review
                    .multibuffer
                    .read(cx)
                    .snapshot(cx)
                    .diff_hunks()
                    .count(),
                2
            );
            let snapshot = review.multibuffer.read(cx).snapshot(cx);
            for (hunk, id) in snapshot.diff_hunks().zip(review.group_blocks(0)) {
                assert_eq!(
                    review.block_at(&review.editor, hunk.multi_buffer_range, cx),
                    Some(id)
                );
            }
        });
        let ids = review.read_with(cx, |review, _| review.group_blocks(0));
        let export = review.update(cx, |review, cx| {
            review.filter = ReviewFilter::Reviewed;
            review.export_markdown(cx)
        });
        cx.simulate_new_path_selection(|directory| {
            assert_eq!(directory, Path::new(path!("/guided-controls")));
            Some(path!("/exported-review.md").into())
        });
        export.await.expect("export review");
        let exported = fs
            .load(Path::new(path!("/exported-review.md")))
            .await
            .expect("read exported review");
        assert_eq!(
            exported,
            "# Guided Review\n\nBranch: `feature` against `dev`\n\n\
             ## 1. Update model\n\nChanges two model behaviors.\n\n\
             **Files**\n\n- `model.txt`\n\n\
             ## 2. Test model\n\nAdds coverage for the model.\n\n\
             **Files**\n\n- `test.txt`\n"
        );
        let export = review.update(cx, |review, cx| review.export_markdown(cx));
        cx.simulate_new_path_selection(|_| None);
        export.await.expect("cancel export");
        let export = review.update(cx, |review, cx| review.export_markdown(cx));
        cx.simulate_new_path_selection(|_| Some(path!("/guided-controls").into()));
        assert!(
            export
                .await
                .expect_err("cannot overwrite a directory")
                .to_string()
                .contains("Could not export guided review")
        );
        review.update_in(cx, |review, window, cx| {
            review.filter = ReviewFilter::All;
            review.set_reviewed(vec![ids[0].clone()], true, window, cx);
            assert_eq!(review.checked(&ids), ToggleState::Indeterminate);
            review.set_reviewed(ids.clone(), true, window, cx);
            assert_eq!(review.checked(&ids), ToggleState::Selected);
            assert_eq!(review.saved.reviewed.len(), 2);
            review.undo(window, cx);
            assert!(review.saved.reviewed.contains(&ids[0]));
            assert!(!review.saved.reviewed.contains(&ids[1]));
            review.filter = ReviewFilter::Reviewed;
            review.show_group(window, cx);
            assert_eq!(
                review
                    .multibuffer
                    .read(cx)
                    .snapshot(cx)
                    .diff_hunks()
                    .count(),
                1
            );
            review.set_reviewed(vec![ids[0].clone()], false, window, cx);
            assert!(review.saved.reviewed.is_empty());
            assert!(review.multibuffer.read(cx).is_empty());
            review.set_reviewed(ids.clone(), true, window, cx);
            review.set_reviewed(ids.clone(), false, window, cx);
            assert_eq!(review.checked(&ids), ToggleState::Unselected);
            review.set_reviewed(vec![ids[1].clone()], true, window, cx);
            review.selected_group = 1;
            review.filter = ReviewFilter::All;
            review.show_group(window, cx);
            assert_eq!(
                review.selected_file.as_ref().map(|path| path.as_unix_str()),
                Some("test.txt")
            );
        });
        cx.run_until_parked();
        let same = workspace
            .update_in(cx, |workspace, window, cx| {
                GuidedReview::open(workspace, window, cx)
            })
            .expect("reuse guide");
        assert_eq!(same, review);
        review.update_in(cx, |review, window, cx| {
            review.regenerate(false, window, cx)
        });
        cx.run_until_parked();
        review.read_with(cx, |review, _| {
            assert!(review.error.is_none(), "{:?}", review.error);
            assert_eq!(review.saved.reviewed, BTreeSet::from_iter([ids[1].clone()]));
        });
        fs.set_branch_name(git_path, Some("another"));
        cx.run_until_parked();
        review.update_in(cx, |review, window, cx| {
            assert!(review.branch_changed(cx));
            review.set_reviewed(ids.clone(), false, window, cx);
            assert_eq!(review.saved.reviewed.len(), 1);
            review.regenerate(false, window, cx);
        });
        cx.run_until_parked();
        review.read_with(cx, |review, cx| {
            assert!(review.error.is_none(), "{:?}", review.error);
            assert_eq!(review.branch, "refs/heads/another");
            assert!(review.saved.reviewed.is_empty());
            assert_eq!(
                review.prompt_editor.read(cx).text(cx),
                "Focus on tests for this branch."
            );
        });
        fs.set_branch_name(git_path, Some("feature"));
        cx.run_until_parked();
        review.update_in(cx, |review, window, cx| {
            review.regenerate(false, window, cx)
        });
        cx.run_until_parked();
        review.read_with(cx, |review, cx| {
            assert_eq!(review.saved.reviewed, BTreeSet::from_iter([ids[1].clone()]));
            assert_eq!(
                review.prompt_editor.read(cx).text(cx),
                "Explain the model changes and their tests."
            );
        });
        let source = project
            .update(cx, |project, cx| {
                project.open_local_buffer(path!("/guided-controls/model.txt"), cx)
            })
            .await
            .expect("source buffer");
        fs.set_head_for_repo(
            git_path,
            &[
                ("model.txt", current.into()),
                ("test.txt", "new test\n".into()),
            ],
            "feature-head",
        );
        cx.run_until_parked();
        review.read_with(cx, |review, cx| assert!(review.stale(cx)));
        let refreshed = cx
            .update(|window, cx| {
                BranchReviewSnapshot::load(project.clone(), repository.clone(), window, cx)
            })
            .await
            .expect("snapshot after repository rescan");
        review.update(cx, |review, cx| {
            review.refresh_snapshot(refreshed);
            assert!(!review.stale(cx));
            assert_eq!(review.saved.model.as_deref(), Some("test-model"));
        });
        source.update(cx, |source, cx| source.set_text("new edit\n", cx));
        let changed = cx
            .update(|window, cx| {
                BranchReviewSnapshot::load(project.clone(), repository.clone(), window, cx)
            })
            .await
            .expect("snapshot after source edit");
        review.update(cx, |review, cx| {
            review.refresh_snapshot(changed);
            assert!(review.stale(cx));
            assert_eq!(
                review
                    .snapshot
                    .as_ref()
                    .expect("captured snapshot")
                    .files
                    .iter()
                    .find(|file| file.path.as_unix_str() == "model.txt")
                    .expect("captured file")
                    .current_text
                    .as_deref(),
                Some(current)
            );
        });
        review.read_with(cx, |review, cx| {
            assert!(review.stale(cx));
            assert_eq!(
                review
                    .files
                    .iter()
                    .find(|file| file.path.as_unix_str() == "model.txt")
                    .expect("review model")
                    .buffer
                    .read(cx)
                    .text(),
                current
            );
        });
        assert_eq!(
            fs.with_git_state(git_path, false, |state| state
                .index_contents
                .get(&RepoPath::new("model.txt").expect("path"))
                .cloned())
                .expect("index state"),
            Some(current.to_owned())
        );
        let prompt_editor = review.read_with(cx, |review, _| review.prompt_editor.clone());
        let edited = "Explain the tests first, then the behavior changes.";
        prompt_editor.update_in(cx, |editor, window, cx| editor.set_text(edited, window, cx));
        cx.run_until_parked();
        review.read_with(cx, |review, cx| {
            assert_eq!(review.saved.prompt.as_deref(), Some(edited));
            assert_eq!(
                review.saved.generated_prompt.as_deref(),
                Some("Explain the model changes and their tests.")
            );
            let request =
                codex::prompt(&review.files, &prompt_editor.read(cx).text(cx)).expect("request");
            assert!(request.starts_with(edited));
            assert!(request.contains("model.txt"));
            assert!(request.contains("TWO"));
            let stored = database
                .read_kvp(&review.storage_key(&review.branch, cx))
                .expect("read saved prompt")
                .expect("saved review");
            let stored: SavedReview = serde_json::from_str(&stored).expect("restore prompt");
            assert_eq!(stored.prompt.as_deref(), Some(edited));
        });
        let mut async_cx = cx.update(|window, cx| window.to_async(cx));
        let terminal = GuidedReview::open_output_terminal(&review.downgrade(), &mut async_cx)
            .expect("open output without borrowing the active guide");
        terminal.update(cx, |terminal, cx| {
            terminal.write_output(b"Generated a concept description.\n", cx)
        });
        cx.run_until_parked();
        assert!(
            terminal
                .read_with(cx, |terminal, _| terminal.get_content())
                .contains("Generated a concept description.")
        );
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(workspace.items_of_type::<GuidedReview>(cx).count(), 1);
            assert_eq!(workspace.items_of_type::<TerminalView>(cx).count(), 1);
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.activate_item(&review, true, true, window, cx);
        });
        let pane_count = workspace.read_with(cx, |workspace, _| workspace.panes().len());
        let diff_editor = review.update_in(cx, |review, window, cx| {
            review.selected_group = 0;
            review.filter = ReviewFilter::All;
            review.show_group(window, cx);
            review.editor.clone()
        });
        let target = review.read_with(cx, |review, cx| {
            let file = review
                .files
                .iter()
                .find(|file| file.path.as_unix_str() == "model.txt")
                .expect("model file");
            let anchor = file.buffer.read(cx).anchor_before(Point::new(10, 3));
            review
                .multibuffer
                .read(cx)
                .snapshot(cx)
                .anchor_in_excerpt(anchor)
                .expect("target in second hunk")
        });
        diff_editor.update_in(cx, |editor, window, cx| {
            editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
                selections.select_ranges([target..target])
            });
            editor.open_excerpts(&editor::actions::OpenExcerpts, window, cx);
        });
        cx.run_until_parked();
        let live_diff = workspace.read_with(cx, |workspace, cx| {
            assert_eq!(workspace.panes().len(), pane_count + 1);
            let opened = workspace.active_item_as::<Editor>(cx).expect("opened file");
            let buffer = opened
                .read(cx)
                .buffer()
                .read(cx)
                .as_singleton()
                .expect("source buffer");
            assert_eq!(buffer, source);
            assert_eq!(buffer.read(cx).text(), "new edit\n");
            assert!(!opened.read(cx).read_only(cx));
            let diff = opened
                .read(cx)
                .buffer()
                .read(cx)
                .diff_for(buffer.read(cx).remote_id())
                .expect("dev diff");
            assert_eq!(diff.read(cx).base_text_string(cx).as_deref(), Some(base));
            assert_eq!(workspace.items_of_type::<GuidedReview>(cx).count(), 1);
            let opened = opened.read(cx);
            let (anchor, _) = opened
                .buffer()
                .read(cx)
                .snapshot(cx)
                .anchor_to_buffer_anchor(opened.selections.newest_anchor().head())
                .expect("source cursor");
            assert_eq!(
                language::ToPoint::to_point(&anchor, &source.read(cx).snapshot()),
                Point::new(1, 0)
            );
            diff
        });
        source.update(cx, |buffer, cx| buffer.set_text(base, cx));
        cx.run_until_parked();
        live_diff.read_with(cx, |diff, cx| {
            assert_eq!(diff.base_text_string(cx).as_deref(), Some(base));
            assert!(
                diff.snapshot(cx)
                    .hunks(&source.read(cx).snapshot())
                    .next()
                    .is_none()
            );
        });
        source.update(cx, |source, cx| source.set_text(current, cx));
        cx.run_until_parked();
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.activate_item(&review, true, true, window, cx);
        });
        diff_editor.update_in(cx, |editor, window, cx| {
            editor.open_excerpts(&editor::actions::OpenExcerpts, window, cx);
        });
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            let opened = workspace
                .active_item_as::<Editor>(cx)
                .expect("reopened file");
            let opened = opened.read(cx);
            let (anchor, _) = opened
                .buffer()
                .read(cx)
                .snapshot(cx)
                .anchor_to_buffer_anchor(opened.selections.newest_anchor().head())
                .expect("source cursor after deleted lines");
            assert_eq!(
                language::ToPoint::to_point(&anchor, &source.read(cx).snapshot()),
                Point::new(10, 3)
            );
        });
        let right_pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.activate_item(&review, true, true, window, cx);
        });
        review.update_in(cx, |review, window, cx| {
            review.selected_group = 1;
            review.show_group(window, cx);
        });
        cx.run_until_parked();
        diff_editor.update_in(cx, |editor, window, cx| {
            editor.open_excerpts(&editor::actions::OpenExcerpts, window, cx);
        });
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(workspace.panes().len(), pane_count + 1);
            assert_eq!(workspace.active_pane(), &right_pane);
            assert_eq!(right_pane.read(cx).items_len(), 2);
            let opened = workspace.active_item_as::<Editor>(cx).expect("second file");
            assert_eq!(opened.read(cx).text(cx), "new test\n");
        });
    }

    #[test]
    fn markdown_export_preserves_literal_titles_descriptions_and_paths() {
        let exported = review_markdown(
            "refs/heads/feature/export",
            &[ConceptGroup {
                title: "Support *literal* names".into(),
                description: "Keep <names> and [paths].\n\nAlso `code`.".into(),
                files: vec!["src/a`b.rs".into(), "docs/my guide.md".into()],
            }],
        );
        assert!(exported.contains("Branch: `feature/export` against `dev`"));
        assert!(exported.contains(r"## 1. Support \*literal\* names"));
        assert!(exported.contains("Keep &lt;names&gt; and \\[paths].\n\nAlso \\`code\\`."));
        assert!(exported.contains("- ``src/a`b.rs``\n- `docs/my guide.md`\n"));
    }

    #[test]
    fn restores_reviews_created_before_descriptions_and_editable_prompts() {
        let saved: SavedReview = serde_json::from_value(json!({
            "groups": [{"title": "Model", "files": ["model.rs"]}],
            "reviewed": ["accepted-block"], "fingerprints": {}, "generated": true
        }))
        .expect("legacy review");
        assert!(saved.groups[0].description.is_empty());
        assert!(saved.prompt.is_none());
        assert!(saved.generated_prompt.is_none());
        assert!(saved.model.is_none());
        assert!(saved.reviewed.contains("accepted-block"));
    }

    #[gpui::test]
    async fn unchanged_blocks_keep_their_identity_after_new_edits(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
        });
        let source = cx.new(|cx| Buffer::local("ONE\na\nb\nc\nd\ne\nf\ng\nh\ni\nTWO\n", cx));
        let source_snapshot = source.read_with(cx, |buffer, _| buffer.snapshot());
        let source_diff = cx.new(|cx| BufferDiff::new(&source_snapshot, cx));
        let file = BranchReviewFile {
            path: RepoPath::new("file.txt").expect("path"),
            base_text: Some("one\na\nb\nc\nd\ne\nf\ng\nh\ni\ntwo\n".into()),
            current_text: Some(source.read_with(cx, |buffer, _| buffer.text())),
            source_snapshot,
            source_diff,
        };
        let before = ReviewFile::new(&file, &mut cx.to_async())
            .await
            .expect("before");
        let changed = BranchReviewFile {
            current_text: Some("INSERTED\nONE\na\nb\nc\nd\ne\nf\ng\nh\ni\nTWO\n".into()),
            ..file
        };
        let after = ReviewFile::new(&changed, &mut cx.to_async())
            .await
            .expect("after");
        assert_eq!(before.blocks.len(), 2);
        assert_ne!(before.blocks[0].id, after.blocks[0].id);
        assert_eq!(before.blocks[1].id, after.blocks[1].id);
    }
}
