//! Launch the isolated agent runtime without a dependency on a system Node install.
use anyhow::{bail, Context, Result};
use clap::Args;
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
};

#[derive(Debug, Args)]
pub struct AgentArgs {
    /// Agent options and commands, such as `exec`, `sessions`, or `--resume`.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<OsString>,
}

#[derive(Debug, Args)]
pub struct ProviderArgs {
    /// Provider command: list, connect, disconnect, test, or models.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<OsString>,
}

pub async fn run_agent(profile: PathBuf, args: AgentArgs, json: bool) -> Result<()> {
    launch(profile, "agent", args.args, json).await
}

pub async fn run_provider(profile: PathBuf, args: ProviderArgs, json: bool) -> Result<()> {
    launch(profile, "provider", args.args, json).await
}

fn bundled_runtime(executable: &Path) -> Option<PathBuf> {
    let parent = executable.parent()?;
    // Installed macOS bundle: Contents/MacOS/dakia and Contents/Resources/agent.
    let candidates = [
        parent.join("../Resources/agent"),
        parent.join("agent"),
        parent.join("../lib/dakia-desktop/agent"),
        parent.join("../lib/dakia/agent"),
    ];
    candidates
        .into_iter()
        .find(|p| p.join("dist/cli.js").is_file() && p.join(node_relative_path()).is_file())
}

fn node_relative_path() -> &'static str {
    if cfg!(windows) {
        "runtime/node.exe"
    } else {
        "runtime/node"
    }
}

async fn launch(profile: PathBuf, command: &str, args: Vec<OsString>, json: bool) -> Result<()> {
    let executable = std::env::current_exe()
        .context("cannot locate the Dakia executable")?
        .canonicalize()
        .context("cannot resolve the Dakia executable")?;
    let (node, entry) = if let Some(bundle) = bundled_runtime(&executable) {
        (
            bundle.join(node_relative_path()).into_os_string(),
            bundle.join("dist/cli.js"),
        )
    } else if cfg!(debug_assertions) {
        // A development-only fallback. Release builds never consult PATH for Node.
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let entry = root.join("packages/dakia-agent/dist/cli.js");
        if !entry.is_file() {
            bail!("The Dakia agent has not been built. Run npm run build:agent in the development checkout.");
        }
        (OsString::from("node"), entry)
    } else {
        bail!("The Dakia agent runtime is missing. Reinstall Dakia to restore it.");
    };
    let mut child = tokio::process::Command::new(node);
    child
        .arg(entry)
        .arg(command)
        .args(args)
        .env("DAKIA_PROFILE_DIR", profile)
        .env("DAKIA_CLI_PATH", executable)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    if json {
        child.arg("--json");
    }
    let status = child
        .status()
        .await
        .context("could not start the Dakia agent")?;
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recognizes_only_complete_relative_runtime() {
        let dir = tempfile::tempdir().unwrap();
        let macos = dir.path().join("Contents/MacOS");
        let agent = dir.path().join("Contents/Resources/agent");
        std::fs::create_dir_all(&macos).unwrap();
        std::fs::create_dir_all(agent.join("dist")).unwrap();
        assert!(bundled_runtime(&macos.join("dakia")).is_none());
        std::fs::write(agent.join("dist/cli.js"), "").unwrap();
        assert!(bundled_runtime(&macos.join("dakia")).is_none());
        std::fs::create_dir_all(agent.join("runtime")).unwrap();
        std::fs::write(agent.join(node_relative_path()), "").unwrap();
        assert!(bundled_runtime(&macos.join("dakia")).is_some());
    }

    #[test]
    fn recognizes_linux_appimage_resource_layout() {
        let dir = tempfile::tempdir().unwrap();
        let executable = dir.path().join("usr/bin/dakia");
        let agent = dir.path().join("usr/lib/dakia-desktop/agent");
        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
        std::fs::create_dir_all(agent.join("dist")).unwrap();
        std::fs::create_dir_all(agent.join("runtime")).unwrap();
        std::fs::write(agent.join("dist/cli.js"), "").unwrap();
        std::fs::write(agent.join(node_relative_path()), "").unwrap();
        assert!(bundled_runtime(&executable).is_some());
    }
}
