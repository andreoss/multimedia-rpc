use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub crate_name: String,
    pub dependency: String,
    pub rule: &'static str,
}

impl std::fmt::Display for Violation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} depends on {} ({})",
            self.crate_name, self.dependency, self.rule
        )
    }
}

#[derive(Debug, Clone)]
pub struct Manifest {
    pub name: String,
    pub dependencies: Vec<String>,
}

const TOOL_CRATES: [&str; 3] = ["mm-ffmpeg", "mm-magick", "mm-gst"];
const DOMAIN_CRATE: &str = "mm-domain";
const ADAPTER_CRATES: [&str; 4] = ["mm-auth", "mm-jobstore", "mm-observability", "mm-storage"];
const SERVER_CRATE: &str = "mm-server";
const WORKER_CRATE: &str = "mm-worker";

pub fn workspace_manifests(root: &Path) -> Result<Vec<Manifest>, String> {
    let root_manifest = fs::read_to_string(root.join("Cargo.toml"))
        .map_err(|error| format!("root manifest: {error}"))?;
    let members = members(&root_manifest);
    let mut manifests = Vec::with_capacity(members.len());
    for member in members {
        let path = root.join(&member).join("Cargo.toml");
        let text =
            fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
        let name =
            package_name(&text).ok_or_else(|| format!("{} has no package name", path.display()))?;
        manifests.push(Manifest {
            name,
            dependencies: dependencies(&text),
        });
    }
    Ok(manifests)
}

pub fn check_layering(root: &Path) -> Result<(), Vec<Violation>> {
    let manifests = match workspace_manifests(root) {
        Ok(manifests) => manifests,
        Err(error) => {
            return Err(vec![Violation {
                crate_name: "workspace".to_string(),
                dependency: error,
                rule: "manifest",
            }]);
        }
    };
    let by_name: BTreeMap<&str, &Manifest> = manifests
        .iter()
        .map(|manifest| (manifest.name.as_str(), manifest))
        .collect();
    let mut violations = Vec::new();
    for manifest in &manifests {
        for dependency in &manifest.dependencies {
            let Some(target) = by_name.get(dependency.as_str()) else {
                continue;
            };
            let violation = if TOOL_CRATES.contains(&manifest.name.as_str()) {
                Some("a tool crate must not depend on the workspace")
            } else if manifest.name == DOMAIN_CRATE {
                Some("a domain crate must not depend on the workspace")
            } else if manifest.name != target.name
                && ADAPTER_CRATES.contains(&manifest.name.as_str())
                && ADAPTER_CRATES.contains(&target.name.as_str())
            {
                Some("an adapter must not depend on another adapter")
            } else if manifest.name == SERVER_CRATE && target.name == WORKER_CRATE {
                Some("a server must not depend on the worker")
            } else {
                None
            };
            if let Some(rule) = violation {
                violations.push(Violation {
                    crate_name: manifest.name.clone(),
                    dependency: target.name.clone(),
                    rule,
                });
            }
        }
    }
    if violations.is_empty() {
        Ok(())
    } else {
        Err(violations)
    }
}

pub fn check_comments(root: &Path) -> Result<usize, Vec<String>> {
    let manifests = match workspace_manifests(root) {
        Ok(manifests) => manifests,
        Err(error) => return Err(vec![error]),
    };
    let mut files = 0usize;
    let mut violations = Vec::new();
    for manifest in &manifests {
        let directory = root.join("crates").join(&manifest.name);
        let directory = if directory.is_dir() {
            directory
        } else {
            root.join(&manifest.name)
        };
        for path in rust_files(&directory) {
            files += 1;
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            for (index, line) in text.lines().enumerate() {
                let trimmed = line.trim_start();
                if trimmed.starts_with("//") || trimmed.starts_with("/*") {
                    violations.push(format!("{}:{}: {}", path.display(), index + 1, trimmed));
                }
            }
        }
    }
    if violations.is_empty() {
        Ok(files)
    } else {
        Err(violations)
    }
}

pub fn run(arguments: &[String]) -> Result<(), String> {
    let root = std::env::current_dir().map_err(|error| error.to_string())?;
    match arguments.first().map(String::as_str) {
        Some("layering") => match check_layering(&root) {
            Ok(()) => {
                println!("layering: ok");
                Ok(())
            }
            Err(violations) => {
                let mut message = String::from("layering: refused\n");
                for violation in violations {
                    let _ = writeln!(message, "  {violation}");
                }
                Err(message)
            }
        },
        Some("comments") => match check_comments(&root) {
            Ok(files) => {
                println!("comments: ok ({files} files)");
                Ok(())
            }
            Err(violations) => {
                let mut message = String::from("comments: refused\n");
                for violation in violations {
                    let _ = writeln!(message, "  {violation}");
                }
                Err(message)
            }
        },
        Some("ci") => ci(&root),
        Some(other) => Err(format!(
            "unknown xtask {other:?}; expected ci, layering or comments"
        )),
        None => Err("usage: cargo xtask <ci|layering|comments>".to_string()),
    }
}

pub fn ci(root: &Path) -> Result<(), String> {
    let database_url = std::env::var("MM_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://mm:mm@127.0.0.1:5433/mm".to_string());
    let base_ref = std::env::var("MM_GATE_BASE_REF").unwrap_or_else(|_| "master".to_string());
    let base_ref = base_ref
        .strip_prefix("origin/")
        .unwrap_or(&base_ref)
        .to_string();
    let threads = std::env::var("MM_GATE_TEST_THREADS").unwrap_or_else(|_| "4".to_string());
    let steps: Vec<(&str, Vec<String>)> = vec![
        (
            "migrate",
            vec![
                "sqlx".into(),
                "migrate".into(),
                "run".into(),
                "--source".into(),
                "crates/mm-jobstore/migrations".into(),
            ],
        ),
        (
            "format",
            vec![
                "cargo".into(),
                "fmt".into(),
                "--all".into(),
                "--check".into(),
            ],
        ),
        (
            "lint",
            vec![
                "cargo".into(),
                "clippy".into(),
                "--workspace".into(),
                "--all-targets".into(),
                "--".into(),
                "-D".into(),
                "warnings".into(),
            ],
        ),
        ("contract", vec!["buf".into(), "lint".into()]),
        (
            "breaking",
            vec![
                "buf".into(),
                "breaking".into(),
                "--against".into(),
                format!(".git#branch={base_ref}"),
            ],
        ),
        (
            "suite",
            vec![
                "cargo".into(),
                "test".into(),
                "--workspace".into(),
                "--no-fail-fast".into(),
                "--".into(),
                format!("--test-threads={threads}"),
            ],
        ),
        (
            "coverage",
            vec![
                "cargo".into(),
                "llvm-cov".into(),
                "--workspace".into(),
                "--no-fail-fast".into(),
                "--fail-under-lines".into(),
                "85".into(),
                "--".into(),
                format!("--test-threads={threads}"),
            ],
        ),
    ];
    for (name, arguments) in steps {
        println!("gate: {name}");
        let status = Command::new(&arguments[0])
            .args(&arguments[1..])
            .current_dir(root)
            .env("DATABASE_URL", &database_url)
            .status()
            .map_err(|error| format!("gate: {name}: {error}"))?;
        if !status.success() {
            return Err(format!("gate: {name}: exited {status}"));
        }
    }
    println!("gate: green");
    Ok(())
}

fn members(manifest: &str) -> Vec<String> {
    let mut members = Vec::new();
    let mut in_members = false;
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("members") {
            in_members = true;
            continue;
        }
        if in_members {
            if trimmed.starts_with(']') {
                break;
            }
            if let Some(value) = quoted(trimmed) {
                members.push(value);
            }
        }
    }
    members
}

fn package_name(manifest: &str) -> Option<String> {
    let mut in_package = false;
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed == "[package]" {
            in_package = true;
            continue;
        }
        if in_package && trimmed.starts_with('[') {
            break;
        }
        if in_package && trimmed.starts_with("name") {
            return quoted(trimmed);
        }
    }
    None
}

fn dependencies(manifest: &str) -> Vec<String> {
    let mut dependencies = Vec::new();
    let mut in_dependencies = false;
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_dependencies = trimmed == "[dependencies]" || trimmed == "[dev-dependencies]";
            continue;
        }
        if in_dependencies && let Some(value) = quoted_name(trimmed) {
            dependencies.push(value);
        }
    }
    dependencies
}

fn quoted(value: &str) -> Option<String> {
    let start = value.find('"')? + 1;
    let rest = &value[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn quoted_name(line: &str) -> Option<String> {
    let name = line.split('=').next()?.trim().trim_matches('"');
    name.starts_with("mm-").then(|| name.to_string())
}

fn rust_files(directory: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut pending = vec![directory.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                files.push(path);
            }
        }
    }
    files
}
