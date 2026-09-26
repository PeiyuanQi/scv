//! The session's turn queue: prompts submitted while a turn runs, which a
//! client may edit, reorder, or remove until they start.

use scv_protocol::{Attachment, ErrorCode, QueueEntry};
use uuid::Uuid;

use super::Session;

pub(crate) const MAX_QUEUE_ITEMS: usize = 64;
pub(crate) const MAX_QUEUE_BYTES: usize = 4 * 1024 * 1024;

impl Session {
    pub(crate) async fn enqueue(
        &self,
        prompt: String,
        submitter: String,
        attachments: Vec<Attachment>,
    ) -> std::result::Result<QueueEntry, ErrorCode> {
        let entry = QueueEntry {
            queue_id: Uuid::new_v4().to_string(),
            revision: 1,
            prompt,
            submitter,
            attachments,
        };
        let mut queue = self.queue.lock().await;
        let bytes: usize = queue.iter().map(|item| item.prompt.len()).sum();
        if queue.len() >= MAX_QUEUE_ITEMS
            || bytes.saturating_add(entry.prompt.len()) > MAX_QUEUE_BYTES
        {
            return Err(ErrorCode::QueueLimit);
        }
        queue.push_back(entry.clone());
        Ok(entry)
    }

    pub(crate) async fn update_queue(
        &self,
        id: &str,
        revision: u64,
        prompt: String,
    ) -> std::result::Result<QueueEntry, ErrorCode> {
        let mut queue = self.queue.lock().await;
        let bytes: usize = queue.iter().map(|item| item.prompt.len()).sum();
        let entry = queue
            .iter_mut()
            .find(|entry| entry.queue_id == id)
            .ok_or(ErrorCode::QueueNotFound)?;
        if entry.revision != revision {
            return Err(ErrorCode::QueueConflict);
        }
        if bytes
            .saturating_sub(entry.prompt.len())
            .saturating_add(prompt.len())
            > MAX_QUEUE_BYTES
        {
            return Err(ErrorCode::QueueLimit);
        }
        entry.prompt = prompt;
        entry.revision += 1;
        Ok(entry.clone())
    }

    pub(crate) async fn move_queue(
        &self,
        session_id: &str,
        id: &str,
        revision: u64,
        before: Option<String>,
    ) -> std::result::Result<(String, u64, usize), ErrorCode> {
        if self.id != session_id {
            return Err(ErrorCode::SessionNotFound);
        }
        let mut queue = self.queue.lock().await;
        let index = queue
            .iter()
            .position(|entry| entry.queue_id == id)
            .ok_or(ErrorCode::QueueNotFound)?;
        if queue[index].revision != revision {
            return Err(ErrorCode::QueueConflict);
        }
        // Validate the destination while the source is still present. This keeps
        // the operation atomic and handles a self move as a no-op reorder.
        let target_index = match before.as_deref() {
            Some(target) if target == id => return Ok((id.to_string(), revision, index)),
            Some(target) => Some(
                queue
                    .iter()
                    .position(|item| item.queue_id == target)
                    .ok_or(ErrorCode::QueueNotFound)?,
            ),
            None => None,
        };
        let mut entry = queue.remove(index).expect("queue index exists");
        let target = target_index.map_or(queue.len(), |target| {
            target.saturating_sub(usize::from(target > index))
        });
        let pos = target.min(queue.len());
        let id = entry.queue_id.clone();
        let rev = entry.revision + 1;
        entry.revision = rev;
        queue.insert(pos, entry);
        Ok((id, rev, pos))
    }

    pub(crate) async fn remove_queue(
        &self,
        session_id: &str,
        id: &str,
        revision: u64,
    ) -> std::result::Result<(String, u64), ErrorCode> {
        if self.id != session_id {
            return Err(ErrorCode::SessionNotFound);
        }
        let mut queue = self.queue.lock().await;
        let index = queue
            .iter()
            .position(|entry| entry.queue_id == id)
            .ok_or(ErrorCode::QueueNotFound)?;
        if queue[index].revision != revision {
            return Err(ErrorCode::QueueConflict);
        }
        let entry = queue.remove(index).expect("queue index exists");
        Ok((entry.queue_id, entry.revision))
    }
}
