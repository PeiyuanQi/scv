//! Keys and the composer: editing, prompt history, slash commands, and the
//! messages keys send to the server.

use std::time::Instant;

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use scv_protocol::ClientMessage;

use crate::{
    app::{ActiveTurn, App},
    client::{Client, is_transport_error, new_id},
    transcript::TranscriptItem,
};

pub(crate) async fn handle_key(client: &mut Client, app: &mut App, key: KeyEvent) -> Result<()> {
    if !app.connected {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            app.quit = true;
        }
        return Ok(());
    }
    if let Err(error) = handle_connected_key(client, app, key).await {
        if is_transport_error(&error) {
            clear_input(app);
        }
        app.handle_connection_error(error)?;
    }
    Ok(())
}

pub(crate) async fn handle_connected_key(
    client: &mut Client,
    app: &mut App,
    key: KeyEvent,
) -> Result<()> {
    if let Some(approval) = &app.pending_approval {
        let approved = match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => Some(true),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Some(false),
            _ => None,
        };
        if let Some(approved) = approved {
            let id = approval.id.clone();
            client
                .send(&ClientMessage::ApprovalResolve {
                    request_id: new_id(),
                    session_id: app.session_id.clone(),
                    approval_id: id,
                    approved,
                })
                .await?;
            app.pending_approval = None;
        }
        return Ok(());
    }

    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('c') => {
                if !app.input.is_empty() {
                    app.input.clear();
                    app.cursor = 0;
                } else if app.turn.is_some() {
                    cancel_turn(client, app).await?;
                } else {
                    app.quit = true;
                }
                return Ok(());
            }
            KeyCode::Char('j') => {
                insert_char(app, '\n');
                return Ok(());
            }
            KeyCode::Char('o') => {
                app.items.update_last(
                    |item| matches!(item, TranscriptItem::Tool { .. }),
                    |item| {
                        if let TranscriptItem::Tool { expanded, .. } = item {
                            *expanded = !*expanded;
                        }
                    },
                );
                return Ok(());
            }
            KeyCode::Char('x') if app.turn.is_some() && app.queue_selected.is_some() => {
                let index = app.queue_selected.unwrap();
                if let Some(entry) = app.queue.get(index).cloned() {
                    client
                        .send(&ClientMessage::QueueRemove {
                            request_id: new_id(),
                            session_id: app.session_id.clone(),
                            queue_id: entry.queue_id,
                            revision: entry.revision,
                        })
                        .await?;
                }
                return Ok(());
            }
            _ => {}
        }
    }

    match key.code {
        KeyCode::Enter => submit_input(client, app).await?,
        KeyCode::Up if key.modifiers.contains(KeyModifiers::ALT) && app.turn.is_some() => {
            let index = app
                .queue_selected
                .unwrap_or_else(|| app.queue.len().saturating_sub(1));
            if let Some(entry) = app.queue.get(index).cloned() {
                app.input = entry.prompt.clone();
                app.cursor = app.input.chars().count();
                app.queue_editing = Some(entry);
                app.queue_selected = Some(index);
            }
        }
        KeyCode::Down if key.modifiers.contains(KeyModifiers::ALT) && app.turn.is_some() => {
            if !app.queue.is_empty() {
                let index = app
                    .queue_selected
                    .map_or(0, |index| (index + 1).min(app.queue.len() - 1));
                app.queue_selected = Some(index);
                if let Some(entry) = app.queue.get(index).cloned() {
                    app.input = entry.prompt.clone();
                    app.cursor = app.input.chars().count();
                    app.queue_editing = Some(entry);
                }
            }
        }
        KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::ALT) && app.turn.is_some() => {
            client
                .send(&ClientMessage::SessionPause {
                    request_id: new_id(),
                    session_id: app.session_id.clone(),
                    paused: !app.queue_paused,
                })
                .await?;
        }
        KeyCode::Char(character) => insert_char(app, character),
        KeyCode::Backspace => backspace(app),
        KeyCode::Delete => delete(app),
        KeyCode::Left => app.cursor = app.cursor.saturating_sub(1),
        KeyCode::Right => app.cursor = (app.cursor + 1).min(app.input.chars().count()),
        KeyCode::Home => app.cursor = 0,
        KeyCode::End => {
            app.cursor = app.input.chars().count();
            app.scroll = 0;
            app.follow_output = true;
        }
        KeyCode::Up if !app.input.contains('\n') => recall_history(app, true),
        KeyCode::Down if !app.input.contains('\n') => recall_history(app, false),
        KeyCode::PageUp => {
            app.scroll = app.scroll.saturating_add(10);
            app.follow_output = false;
        }
        KeyCode::PageDown => {
            app.scroll = app.scroll.saturating_sub(10);
            app.follow_output = app.scroll == 0;
        }
        KeyCode::Esc if app.turn.is_some() => cancel_turn(client, app).await?,
        _ => {}
    }
    Ok(())
}

pub(crate) async fn submit_input(client: &mut Client, app: &mut App) -> Result<()> {
    let prompt = app.input.trim().to_owned();
    if prompt.is_empty() {
        return Ok(());
    }
    match prompt.as_str() {
        "/quit" => {
            app.quit = true;
            return Ok(());
        }
        "/help" => {
            app.push_item(TranscriptItem::System(
                "Enter send · Ctrl+J newline · Esc cancel · Ctrl+O tool details · PageUp/PageDown scroll · /clear · /context · /quit".into(),
            ));
            clear_input(app);
            return Ok(());
        }
        "/context" => {
            let selected = app.context_after_tokens.map_or_else(
                || "not compacted".into(),
                |value| format!("~{value} selected"),
            );
            let history = app
                .history_bytes
                .map_or_else(|| "within limit".into(), |value| format!("{value} bytes"));
            app.push_item(TranscriptItem::System(format!(
                "Context: {selected} / {} tokens · history {history}",
                app.context_max_tokens
            )));
            clear_input(app);
            return Ok(());
        }
        "/clear" => {
            client
                .send(&ClientMessage::SessionClear {
                    request_id: new_id(),
                    session_id: app.session_id.clone(),
                })
                .await?;
            clear_input(app);
            return Ok(());
        }
        _ => {}
    }
    if let Some(entry) = app.queue_editing.take() {
        client
            .send(&ClientMessage::QueueUpdate {
                request_id: new_id(),
                session_id: app.session_id.clone(),
                queue_id: entry.queue_id,
                revision: entry.revision,
                prompt,
            })
            .await?;
    } else {
        client
            .send(&ClientMessage::TurnStart {
                request_id: new_id(),
                session_id: app.session_id.clone(),
                prompt: prompt.clone(),
                attachments: Vec::new(),
            })
            .await?;
        if app.turn.is_none() {
            app.push_item(TranscriptItem::User(prompt.clone()));
            app.add_prompt_history(prompt);
            app.turn = Some(ActiveTurn {
                id: None,
                started_at: Instant::now(),
            });
        }
        app.queue_selected = None;
    }
    clear_input(app);
    Ok(())
}

pub(crate) async fn cancel_turn(client: &mut Client, app: &mut App) -> Result<()> {
    if let Some(turn_id) = app.turn.as_ref().and_then(|turn| turn.id.as_ref()) {
        client
            .send(&ClientMessage::TurnCancel {
                request_id: new_id(),
                session_id: app.session_id.clone(),
                turn_id: turn_id.clone(),
            })
            .await?;
    }
    Ok(())
}

pub(crate) fn clear_input(app: &mut App) {
    app.input.clear();
    app.cursor = 0;
    app.history_index = None;
}

pub(crate) fn insert_char(app: &mut App, character: char) {
    let index = byte_index(&app.input, app.cursor);
    app.input.insert(index, character);
    app.cursor += 1;
    app.history_index = None;
}

pub(crate) fn backspace(app: &mut App) {
    if app.cursor == 0 {
        return;
    }
    let end = byte_index(&app.input, app.cursor);
    let start = byte_index(&app.input, app.cursor - 1);
    app.input.replace_range(start..end, "");
    app.cursor -= 1;
}

pub(crate) fn delete(app: &mut App) {
    if app.cursor >= app.input.chars().count() {
        return;
    }
    let start = byte_index(&app.input, app.cursor);
    let end = byte_index(&app.input, app.cursor + 1);
    app.input.replace_range(start..end, "");
}

pub(crate) fn byte_index(value: &str, char_index: usize) -> usize {
    value
        .char_indices()
        .nth(char_index)
        .map_or(value.len(), |(index, _)| index)
}

pub(crate) fn recall_history(app: &mut App, older: bool) {
    if app.prompt_history.is_empty() {
        return;
    }
    let next = match (app.history_index, older) {
        (None, true) => app.prompt_history.len() - 1,
        (Some(index), true) => index.saturating_sub(1),
        (Some(index), false) if index + 1 < app.prompt_history.len() => index + 1,
        (_, false) => {
            clear_input(app);
            return;
        }
    };
    app.history_index = Some(next);
    app.input = app.prompt_history[next].clone();
    app.cursor = app.input.chars().count();
}
