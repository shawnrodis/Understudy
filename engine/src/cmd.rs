//! Command execution as the engine's user.
//!
//! This is the "any possible thing" endpoint: it runs a command with the
//! engine's user privileges and returns stdout, stderr, and the exit code.
//! It is gated by the same Tailscale-only listener + bearer token as every
//! other endpoint, and every invocation is action-logged with the full
//! command text. There is no sandboxing here by design — the owner asked
//! for full user-level control — which is exactly why the transport and
//! auth layers around it are non-optional (see README and SECURITY.md).

use std::process::Command;

#[derive(Debug)]
pub struct CmdOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

/// Run `command` with `args` in `cwd` (if given). Windows: `cmd /C`.
/// Other platforms: `sh -c` with the command string (dev path only).
pub fn run(command: &str, args: &[String], cwd: Option<&str>) -> Result<CmdOutput, String> {
    #[cfg(windows)]
    {
        let mut cmd = Command::new("cmd");
        cmd.arg("/C").arg(command).args(args);
        run_inner(cmd, cwd)
    }
    #[cfg(not(windows))]
    {
        let mut full = command.to_string();
        for a in args {
            full.push(' ');
            full.push_str(a);
        }
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(full);
        run_inner(cmd, cwd)
    }
}

fn run_inner(mut cmd: Command, cwd: Option<&str>) -> Result<CmdOutput, String> {
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let out = cmd
        .output()
        .map_err(|e| format!("cannot run command: {}", e))?;
    Ok(CmdOutput {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        exit_code: out.status.code().unwrap_or(-1),
    })
}
