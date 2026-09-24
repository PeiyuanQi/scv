//! The `bash` tool: one `bash -lc` command in the workspace, in its own
//! process group, bounded by a timeout and an output limit.

use std::{ffi::OsString, time::Duration};

use async_trait::async_trait;
use scv_core::{Tool, ToolContext, ToolError, ToolOutput, ToolRisk, ToolSpec};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    args::{Timeouts, bounded, parse_args, timeout_schema, validate_process_args},
    process::{ProcessSpec, execute_process},
};

pub(crate) struct BashTool {
    pub(crate) timeout: Duration,
    pub(crate) max_timeout: Duration,
    pub(crate) output_limit: usize,
}

impl BashTool {
    fn timeouts(&self) -> Timeouts {
        Timeouts {
            default: self.timeout,
            max: self.max_timeout,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BashArgs {
    command: String,
    timeout_seconds: Option<u64>,
}

#[async_trait]
impl Tool for BashTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bash".into(),
            description: "Run a Bash command in the workspace (not sandboxed)".into(),
            parameters: json!({
                "type":"object",
                "properties":{
                    "command":{"type":"string"},
                    "timeout_seconds":timeout_schema(self.timeouts())
                },
                "required":["command"],
                "additionalProperties":false
            }),
        }
    }

    fn risk(&self, arguments: &Value) -> Result<ToolRisk, ToolError> {
        let args: BashArgs = parse_args(arguments)?;
        validate_process_args(&args.command)?;
        self.timeouts().resolve(args.timeout_seconds)?;
        Ok(ToolRisk::Process)
    }

    fn approval_summary(&self, arguments: &Value) -> Result<String, ToolError> {
        let args: BashArgs = parse_args(arguments)?;
        validate_process_args(&args.command)?;
        self.timeouts().resolve(args.timeout_seconds)?;
        Ok(format!(
            "Run with /bin/bash -lc: {}",
            bounded(&args.command, 2000)
        ))
    }

    async fn execute(
        &self,
        arguments: Value,
        context: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let args: BashArgs = parse_args(&arguments)?;
        validate_process_args(&args.command)?;
        let requested = self.timeouts().resolve(args.timeout_seconds)?;
        execute_process(
            ProcessSpec {
                executable: OsString::from("/bin/bash"),
                args: vec![OsString::from("-lc"), OsString::from(args.command)],
                cwd: context.workspace,
                environment: Vec::new(),
                sanitize_scv_environment: false,
                timeout: requested,
                output_limit: self.output_limit,
            },
            context.cancellation,
        )
        .await
    }
}

#[cfg(test)]
mod tests;
