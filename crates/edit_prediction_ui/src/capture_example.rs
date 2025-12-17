use anyhow::{Context as _, Result};
use edit_prediction::{EditPredictionStore, example_spec::ExampleSpec};
use editor::Editor;
use git::repository::DiffType;
use gpui::{Task, Window, prelude::*};
use language::ToPoint as _;
use log;
use std::{path::Path, sync::Arc};
use text::ToOffset as _;
use workspace::Workspace;

pub(crate) fn capture_example(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Result<Task<Result<ExampleSpec>>> {
    let ep_store =
        EditPredictionStore::try_global(cx).context("no edit prediction store initialized")?;

    let project = workspace.project().clone();

    let (worktree_root, repository) = {
        let project_ref = project.read(cx);
        let worktree_root = project_ref
            .visible_worktrees(cx)
            .next()
            .map(|worktree| worktree.read(cx).abs_path());
        let repository = project_ref.active_repository(cx);
        (worktree_root, repository)
    };

    let (Some(worktree_root), Some(repository)) = (worktree_root, repository) else {
        anyhow::bail!("missing worktree or active repository");
    };

    let repository_snapshot = repository.read(cx).snapshot();
    if worktree_root.as_ref() != repository_snapshot.work_directory_abs_path.as_ref() {
        anyhow::bail!(
            "repository is not at worktree root (repo={:?}, worktree={:?})",
            repository_snapshot.work_directory_abs_path,
            worktree_root
        );
    }

    let repository_url = repository_snapshot
        .remote_origin_url
        .clone()
        .or_else(|| repository_snapshot.remote_upstream_url.clone())
        .context("active repository has no origin/upstream remote url")?;

    let revision = repository_snapshot
        .head_commit
        .as_ref()
        .map(|commit| commit.sha.to_string())
        .context("active repository has no head commit")?;

    let mut events = ep_store.update(cx, |store, cx| {
        store.edit_history_for_project_with_pause_split_last_event(&project, cx)
    });

    let editor = workspace
        .active_item_as::<Editor>(cx)
        .context("no active editor")?;

    let project_path = editor
        .read(cx)
        .project_path(cx)
        .context("active editor has no project path")?;

    let (buffer, cursor_anchor) = editor
        .read(cx)
        .buffer()
        .read(cx)
        .text_anchor_for_position(editor.read(cx).selections.newest_anchor().head(), cx)
        .context("failed to resolve cursor buffer/anchor")?;

    let snapshot = buffer.read(cx).snapshot();
    let cursor_point = cursor_anchor.to_point(&snapshot);
    let (_editable_range, context_range) =
        edit_prediction::cursor_excerpt::editable_and_context_ranges_for_cursor_position(
            cursor_point,
            &snapshot,
            100,
            50,
        );

    let cursor_path: Arc<Path> = repository
        .read(cx)
        .project_path_to_repo_path(&project_path, cx)
        .map(|repo_path| Path::new(repo_path.as_unix_str()).into())
        .unwrap_or_else(|| Path::new(project_path.path.as_unix_str()).into());

    let cursor_position = {
        let context_start_offset = context_range.start.to_offset(&snapshot);
        let cursor_offset = cursor_anchor.to_offset(&snapshot);
        let cursor_offset_in_excerpt = cursor_offset.saturating_sub(context_start_offset);
        let mut excerpt = snapshot.text_for_range(context_range).collect::<String>();
        if cursor_offset_in_excerpt <= excerpt.len() {
            excerpt.insert_str(cursor_offset_in_excerpt, zeta_prompt::CURSOR_MARKER);
        }
        excerpt
    };

    Ok(cx.spawn_in(window, async move |_workspace_entity, cx| {
        let uncommitted_diff_rx = repository.update(cx, |repository, cx| {
            repository.diff(DiffType::HeadToWorktree, cx)
        })?;

        let uncommitted_diff = match uncommitted_diff_rx.await {
            Ok(Ok(diff)) => diff,
            Ok(Err(error)) => {
                anyhow::bail!("failed to compute uncommitted diff: {error:#}");
            }
            Err(error) => {
                anyhow::bail!("uncommitted diff channel dropped: {error:#}");
            }
        };

        let mut edit_history = String::new();
        let mut expected_patch = String::new();
        if let Some(last_event) = events.pop() {
            for event in &events {
                zeta_prompt::write_event(&mut edit_history, event);
                if !edit_history.ends_with('\n') {
                    edit_history.push('\n');
                }
                edit_history.push('\n');
            }

            zeta_prompt::write_event(&mut expected_patch, &last_event);
        }

        let format =
            time::format_description::parse("[year]-[month]-[day] [hour]:[minute]:[second]");
        let name = match format {
            Ok(format) => {
                let now = time::OffsetDateTime::now_local()
                    .unwrap_or_else(|_| time::OffsetDateTime::now_utc());
                now.format(&format)
                    .unwrap_or_else(|_| "unknown-time".to_string())
            }
            Err(_) => "unknown-time".to_string(),
        };

        Ok(ExampleSpec {
            name,
            repository_url,
            revision,
            uncommitted_diff,
            cursor_path,
            cursor_position,
            edit_history,
            expected_patch,
        })
    }))
}

pub(crate) fn capture_example_as_markdown(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let markdown_language = workspace
        .app_state()
        .languages
        .language_for_name("Markdown");

    let example = match capture_example(workspace, window, cx) {
        Ok(task) => task,
        Err(error) => {
            log::error!("failed to capture edit prediction example: {error:#}");
            return;
        }
    };

    let project = workspace.project().clone();

    cx.spawn_in(window, async move |workspace_entity, cx| {
        let markdown_language = markdown_language.await?;
        let example_spec = example.await?;
        let markdown = example_spec.to_markdown();

        let buffer = project
            .update(cx, |project, cx| project.create_buffer(false, cx))?
            .await?;
        buffer.update(cx, |buffer, cx| {
            buffer.set_text(markdown, cx);
            buffer.set_language(Some(markdown_language), cx);
        })?;

        workspace_entity.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(
                Box::new(
                    cx.new(|cx| Editor::for_buffer(buffer, Some(project.clone()), window, cx)),
                ),
                None,
                true,
                window,
                cx,
            );
        })
    })
    .detach_and_log_err(cx);
}
