use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

pub const MCP_CONFIG_VERSION: u32 = 1;
const MAX_CONFIG_BYTES: u64 = 64 * 1024;
const MAX_SERVERS: usize = 32;
const MAX_ARGS: usize = 128;
const MAX_ENV_NAMES: usize = 64;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpConfig {
    pub version: u32,
    pub servers: Vec<McpServerConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerConfig {
    pub name: String,
    pub enabled: bool,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub working_directory: Option<PathBuf>,
    #[serde(default)]
    pub env_allowlist: Vec<String>,
}

impl McpConfig {
    pub fn from_env() -> Result<Option<Self>> {
        let Some(raw_path) = std::env::var_os("MCP_CONFIG_PATH") else {
            return Ok(None);
        };
        if raw_path.is_empty() {
            bail!("MCP_CONFIG_PATH must not be empty");
        }
        let path = PathBuf::from(raw_path);
        Ok(Some(Self::from_path(&path)?))
    }

    fn from_path(path: &Path) -> Result<Self> {
        if !path.is_absolute() {
            bail!("MCP_CONFIG_PATH must be an absolute path");
        }
        let metadata = fs::symlink_metadata(path)
            .with_context(|| format!("failed to inspect MCP config `{}`", path.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!("MCP config must be a regular file and not a symlink");
        }
        if metadata.len() > MAX_CONFIG_BYTES {
            bail!("MCP config exceeds the {MAX_CONFIG_BYTES}-byte limit");
        }
        let bytes = fs::read(path)
            .with_context(|| format!("failed to read MCP config `{}`", path.display()))?;
        if bytes.len() as u64 > MAX_CONFIG_BYTES {
            bail!("MCP config exceeds the {MAX_CONFIG_BYTES}-byte limit");
        }
        let config: Self =
            serde_json::from_slice(&bytes).context("MCP config is not valid strict JSON")?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != MCP_CONFIG_VERSION {
            bail!(
                "unsupported MCP config version {}; expected {MCP_CONFIG_VERSION}",
                self.version
            );
        }
        if self.servers.len() > MAX_SERVERS {
            bail!("MCP config contains more than {MAX_SERVERS} servers");
        }
        let mut names = HashSet::new();
        for server in &self.servers {
            validate_component(&server.name, "server name")?;
            if !names.insert(server.name.as_str()) {
                bail!("duplicate MCP server name `{}`", server.name);
            }
            server.validate()?;
        }
        Ok(())
    }
}

impl McpServerConfig {
    fn validate(&self) -> Result<()> {
        if self.command.trim().is_empty() || self.command.contains('\0') {
            bail!(
                "MCP server `{}` has an empty or NUL-containing command",
                self.name
            );
        }
        let command = PathBuf::from(&self.command);
        if !command.is_absolute() {
            bail!(
                "MCP server `{}` command must be an absolute path",
                self.name
            );
        }
        let metadata = fs::metadata(&command)
            .with_context(|| format!("MCP server `{}` command does not exist", self.name))?;
        if !metadata.is_file() {
            bail!("MCP server `{}` command must be a file", self.name);
        }
        if self.args.len() > MAX_ARGS || self.args.iter().any(|arg| arg.contains('\0')) {
            bail!(
                "MCP server `{}` has too many arguments or a NUL argument",
                self.name
            );
        }
        if let Some(directory) = &self.working_directory {
            if !directory.is_absolute() {
                bail!(
                    "MCP server `{}` working_directory must be absolute",
                    self.name
                );
            }
            let metadata = fs::metadata(directory).with_context(|| {
                format!(
                    "MCP server `{}` working_directory does not exist",
                    self.name
                )
            })?;
            if !metadata.is_dir() {
                bail!(
                    "MCP server `{}` working_directory must be a directory",
                    self.name
                );
            }
        }
        if self.env_allowlist.len() > MAX_ENV_NAMES {
            bail!("MCP server `{}` has too many environment names", self.name);
        }
        let mut names = HashSet::new();
        for name in &self.env_allowlist {
            if !valid_env_name(name) {
                bail!(
                    "MCP server `{}` has invalid environment name `{name}`",
                    self.name
                );
            }
            if !names.insert(name.as_str()) {
                bail!(
                    "MCP server `{}` repeats environment name `{name}`",
                    self.name
                );
            }
        }
        Ok(())
    }
}

pub(crate) fn validate_component(value: &str, label: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 48
        || value.contains("__")
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        bail!("invalid MCP {label} `{value}`; use 1-48 ASCII letters, digits, `_`, or `-`");
    }
    Ok(())
}

fn valid_env_name(value: &str) -> bool {
    let mut bytes = value.bytes();
    matches!(bytes.next(), Some(b'A'..=b'Z' | b'a'..=b'z' | b'_'))
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command() -> String {
        std::env::current_exe().unwrap().display().to_string()
    }

    fn server(name: &str) -> McpServerConfig {
        McpServerConfig {
            name: name.into(),
            enabled: true,
            command: command(),
            args: vec![],
            working_directory: None,
            env_allowlist: vec!["HOME".into()],
        }
    }

    #[test]
    fn strict_config_rejects_duplicates_bad_names_and_unknown_fields() {
        let duplicate = McpConfig {
            version: 1,
            servers: vec![server("same"), server("same")],
        };
        assert!(duplicate.validate().is_err());
        let mut invalid = McpConfig {
            version: 1,
            servers: vec![server("bad name")],
        };
        assert!(invalid.validate().is_err());
        invalid.servers[0].name = "ok".into();
        invalid.servers[0].env_allowlist = vec!["A=B".into()];
        assert!(invalid.validate().is_err());
        invalid.servers[0].env_allowlist = vec![];
        invalid.servers[0].command = "relative-server".into();
        assert!(invalid.validate().is_err());
        invalid.servers[0].command = command();
        invalid.servers[0].working_directory = Some(PathBuf::from("relative-directory"));
        assert!(invalid.validate().is_err());
        assert!(
            serde_json::from_str::<McpConfig>(r#"{"version":1,"servers":[],"unknown":true}"#)
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_and_oversized_config_files() {
        use crate::test_support::TestWorkspace;
        use std::os::unix::fs::symlink;

        let workspace = TestWorkspace::new("mcp-config");
        let config = workspace.path().join("config.json");
        fs::write(&config, r#"{"version":1,"servers":[]}"#).unwrap();
        assert!(McpConfig::from_path(&config).is_ok());

        let link = workspace.path().join("config-link.json");
        symlink(&config, &link).unwrap();
        assert!(McpConfig::from_path(&link).is_err());

        let oversized = workspace.path().join("oversized.json");
        fs::write(&oversized, vec![b' '; MAX_CONFIG_BYTES as usize + 1]).unwrap();
        assert!(McpConfig::from_path(&oversized).is_err());
    }

    #[test]
    fn validates_safe_minimal_config() {
        McpConfig {
            version: 1,
            servers: vec![server("local_1")],
        }
        .validate()
        .unwrap();
    }
}
