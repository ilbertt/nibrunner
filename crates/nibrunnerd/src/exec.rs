//! Running the host tools this daemon spawns.
//!
//! `nft` and `mke2fs` on any host. Everything else it needs — the tap, the netlink, the squashfs,
//! the hypervisor — it does itself or carries, so a host's dependency list is two packages rather
//! than a paragraph.
//!
//! A host whose volumes live in an object store adds three more, all of them only for a host that
//! asked for it in `volumes.backend`:
//!
//! - `nbd-client`, because attaching an export is a fork that holds `NBD_DO_IT` for the life of
//!   the device and not a call this daemon could make and return from.
//! - `zerofs`, whose admin CLI is the only interface a service this daemon does not own exposes,
//!   and which is also the read-only server an export's checkpoint is read through.
//! - `debugfs`, which walks a tenant's filesystem in userspace. This one is not a convenience: an
//!   export is built from a filesystem the host must never ask its kernel to interpret, and
//!   mounting it — even read-only — would give that up. It ships in `e2fsprogs` beside `mke2fs`,
//!   so it costs a host no package it did not already have.

use std::process::Stdio;

use async_trait::async_trait;
use tokio::io::AsyncWriteExt;

use crate::services::{CommandError, CommandRequest, CommandResult, CommandRunner};

pub struct HostCommands;

#[async_trait]
impl CommandRunner for HostCommands {
    async fn run(&self, request: CommandRequest) -> Result<CommandResult, CommandError> {
        let executable = request.executable().to_string();
        let unstartable = |reason: String| CommandError::Unstartable {
            executable: executable.clone(),
            reason,
        };

        let mut command = tokio::process::Command::new(&request.command[0]);
        command
            .args(&request.command[1..])
            .stdin(if request.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|error| unstartable(error.to_string()))?;

        if let Some(stdin) = &request.stdin {
            let Some(mut pipe) = child.stdin.take() else {
                return Err(unstartable("its stdin could not be opened".into()));
            };
            pipe.write_all(stdin.as_bytes())
                .await
                .map_err(|error| unstartable(error.to_string()))?;
            // Dropped rather than left open: a tool reading a ruleset off stdin waits for the end
            // of it, so a pipe nobody closed is a process nobody ever hears from.
            drop(pipe);
        }

        let finished = tokio::time::timeout(request.timeout, child.wait_with_output()).await;
        // The process is signalled by the drop above on the way out, so what outlives being given
        // up on is only what could not have been killed by waiting either.
        let output = finished
            .map_err(|_| CommandError::TimedOut {
                executable: executable.clone(),
            })?
            .map_err(|error| unstartable(error.to_string()))?;
        Ok(CommandResult {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn what_a_tool_wrote_comes_back_with_the_code_it_ended_on() {
        let result = HostCommands
            .run(CommandRequest::new(&[
                "sh",
                "-c",
                "echo out; echo err >&2; exit 3",
            ]))
            .await
            .unwrap();
        assert_eq!(result.code, 3);
        assert_eq!(result.stdout.trim(), "out");
        assert_eq!(result.stderr.trim(), "err");
    }

    /// A ruleset is handed to `nft` on stdin, so the pipe closing is what makes the tool answer.
    #[tokio::test]
    async fn what_is_fed_on_stdin_reaches_the_tool_and_the_pipe_is_closed_behind_it() {
        let result = HostCommands
            .stdout_of(CommandRequest::new(&["cat"]).with_stdin("table ip nibrun\n"))
            .await
            .unwrap();
        assert_eq!(result, "table ip nibrun\n");
    }

    #[tokio::test]
    async fn a_tool_that_is_not_there_and_one_that_never_finishes_are_told_apart() {
        let missing = HostCommands
            .run(CommandRequest::new(&["nibrunner-no-such-tool"]))
            .await
            .unwrap_err();
        assert!(matches!(missing, CommandError::Unstartable { .. }));
        let mut slow = CommandRequest::new(&["sleep", "30"]);
        slow.timeout = Duration::from_millis(50);
        assert!(matches!(
            HostCommands.run(slow).await.unwrap_err(),
            CommandError::TimedOut { .. }
        ));
    }
}
