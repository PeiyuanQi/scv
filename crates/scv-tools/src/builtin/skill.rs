//! The `read_skill` tool: loads a discovered skill by name, only from
//! inside its configured roots.

use std::{io::Read as _, path::PathBuf};

use async_trait::async_trait;
use scv_core::{Tool, ToolContext, ToolError, ToolOutput, ToolRisk, ToolSpec};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{SkillMap, args::parse_args};

pub(crate) struct ReadSkillTool {
    pub(crate) skills: SkillMap,
    pub(crate) roots: Vec<PathBuf>,
    pub(crate) max_bytes: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadSkillArgs {
    name: String,
}

#[async_trait]
impl Tool for ReadSkillTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_skill".into(),
            description: "Load a discovered SCV skill by name".into(),
            parameters: json!({
                "type":"object",
                "properties":{"name":{"type":"string"}},
                "required":["name"],
                "additionalProperties":false
            }),
        }
    }

    fn risk(&self, arguments: &Value) -> Result<ToolRisk, ToolError> {
        let _: ReadSkillArgs = parse_args(arguments)?;
        Ok(ToolRisk::ReadOnly)
    }

    fn approval_summary(&self, arguments: &Value) -> Result<String, ToolError> {
        let args: ReadSkillArgs = parse_args(arguments)?;
        Ok(format!("Load skill {}", args.name))
    }

    async fn execute(
        &self,
        arguments: Value,
        context: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let args: ReadSkillArgs = parse_args(&arguments)?;
        let configured = self
            .skills
            .get(&args.name)
            .ok_or_else(|| ToolError(format!("unknown skill: {}", args.name)))?;
        let path = std::fs::canonicalize(configured)
            .map_err(|error| ToolError(format!("load skill {}: {error}", args.name)))?;
        if !self.roots.iter().any(|root| path.starts_with(root)) {
            return Err(ToolError("skill path escaped its configured root".into()));
        }
        let max_bytes = self.max_bytes;
        let skill_name = args.name.clone();
        let bytes = tokio::select! {
            result = tokio::task::spawn_blocking(move || {
                let mut file = std::fs::File::open(&path)
                    .map_err(|error| ToolError(format!("load skill {skill_name}: {error}")))?;
                let mut bytes = Vec::with_capacity(max_bytes.min(8192));
                std::io::Read::take(
                    &mut file,
                    u64::try_from(max_bytes).unwrap_or(u64::MAX).saturating_add(1),
                )
                .read_to_end(&mut bytes)
                .map_err(|error| ToolError(format!("load skill {skill_name}: {error}")))?;
                Ok::<_, ToolError>(bytes)
            }) => result.map_err(|error| ToolError(format!("skill read task failed: {error}")))??,
            () = context.cancellation.cancelled() => return Err(ToolError("skill read cancelled".into())),
        };
        let end = bytes.len().min(self.max_bytes);
        let content = std::str::from_utf8(&bytes[..end])
            .map_err(|_| ToolError("skill is not UTF-8".into()))?;
        Ok(ToolOutput {
            content: content.to_owned(),
            is_error: false,
            truncated: end < bytes.len(),
        })
    }
}
