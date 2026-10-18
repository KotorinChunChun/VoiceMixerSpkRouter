use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{
    error::{BackendError, Result},
    plugins::{
        manifest::{PluginDescriptor, PluginExecutionMode},
        process_host::ProcessPluginFactory,
    },
};

use super::manager::PluginManager;

pub const PLUGIN_MANIFEST_FILE: &str = "plugin.json";

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProcessPluginLaunch {
    pub executable: PathBuf,
    #[serde(default)]
    pub arguments: Vec<String>,
    #[serde(default = "default_start_timeout_ms")]
    pub start_timeout_ms: u64,
}

fn default_start_timeout_ms() -> u64 {
    10_000
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PluginPackageManifest {
    #[serde(flatten)]
    pub descriptor: PluginDescriptor,
    pub process: Option<ProcessPluginLaunch>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PluginDiscoveryReport {
    pub loaded: Vec<String>,
    pub errors: Vec<String>,
}

impl PluginDiscoveryReport {
    fn merge(&mut self, other: Self) {
        self.loaded.extend(other.loaded);
        self.errors.extend(other.errors);
    }
}

pub fn load_package_manifest(path: impl AsRef<Path>) -> Result<PluginPackageManifest> {
    let path = path.as_ref();
    let bytes = fs::read(path).map_err(|source| BackendError::ConfigIo {
        path: path.to_path_buf(),
        source,
    })?;
    let package: PluginPackageManifest = serde_json::from_slice(&bytes)?;
    if package.descriptor.plugin_id.trim().is_empty() {
        return Err(BackendError::Plugin(format!(
            "{}: plugin_id must not be empty",
            path.display()
        )));
    }
    if package.descriptor.name.trim().is_empty() {
        return Err(BackendError::Plugin(format!(
            "{}: name must not be empty",
            path.display()
        )));
    }
    package
        .descriptor
        .validate()
        .map_err(|error| BackendError::Plugin(format!("{}: {error}", path.display())))?;
    Ok(package)
}

/// Discovers external process plugins. A search root may itself contain
/// `plugin.json`, or it may contain immediate child directories that each do.
/// Discovery deliberately does not recurse without bounds.
pub fn discover_process_plugins(
    manager: &PluginManager,
    search_roots: &[PathBuf],
) -> PluginDiscoveryReport {
    let mut report = PluginDiscoveryReport::default();
    for root in search_roots {
        report.merge(discover_search_root(manager, root));
    }
    report.loaded.sort();
    report.loaded.dedup();
    report
}

fn discover_search_root(manager: &PluginManager, root: &Path) -> PluginDiscoveryReport {
    let mut report = PluginDiscoveryReport::default();
    if !root.exists() {
        return report;
    }

    if root.is_file() {
        if root.file_name().and_then(|x| x.to_str()) == Some(PLUGIN_MANIFEST_FILE) {
            register_manifest(manager, root, &mut report);
        }
        return report;
    }

    let direct = root.join(PLUGIN_MANIFEST_FILE);
    if direct.is_file() {
        register_manifest(manager, &direct, &mut report);
    }

    let entries = match fs::read_dir(root) {
        Ok(x) => x,
        Err(err) => {
            report.errors.push(format!(
                "cannot enumerate plugin search path {}: {err}",
                root.display()
            ));
            return report;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let manifest = path.join(PLUGIN_MANIFEST_FILE);
            if manifest.is_file() {
                register_manifest(manager, &manifest, &mut report);
            }
        }
    }
    report
}

fn register_manifest(
    manager: &PluginManager,
    manifest_path: &Path,
    report: &mut PluginDiscoveryReport,
) {
    let result = (|| -> Result<String> {
        let package = load_package_manifest(manifest_path)?;
        if package.descriptor.execution_mode != PluginExecutionMode::Process {
            return Err(BackendError::Plugin(format!(
                "{}: external package execution_mode must currently be 'process'",
                manifest_path.display()
            )));
        }
        if manager.descriptor(&package.descriptor.plugin_id).is_ok() {
            return Ok(package.descriptor.plugin_id);
        }
        let launch = package.process.ok_or_else(|| {
            BackendError::Plugin(format!(
                "{}: process execution mode requires a 'process' launch section",
                manifest_path.display()
            ))
        })?;
        let package_dir = manifest_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        let plugin_id = package.descriptor.plugin_id.clone();
        manager.register_factory(ProcessPluginFactory::shared(
            package.descriptor,
            launch,
            package_dir,
        ))?;
        Ok(plugin_id)
    })();
    match result {
        Ok(id) => report.loaded.push(id),
        Err(err) => report.errors.push(err.to_string()),
    }
}
