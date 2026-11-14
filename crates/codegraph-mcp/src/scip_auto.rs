// ABOUTME: Generates SCIP indexes for detected languages so indexing needs no per-file language-server rounds.
// ABOUTME: Caches indexes by source fingerprint and skips a language when its indexer is unavailable.
use anyhow::{Context, Result, bail};
use codegraph_core::artifact_cache::{ArtifactCache, fingerprint};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};
use tracing::{info, warn};

pub const CACHE_NAMESPACE: &str = "scip-auto-v1";

/// Directories that never contain sources we should fingerprint, discover
/// manifests in, or let an indexer wander into.
const FINGERPRINT_EXCLUDED_DIRECTORIES: &[&str] =
    &["target", "node_modules", ".git", "dist", "build"];
/// `go.mod` discovery skips vendor trees as well: those modules are dependencies.
const GO_EXCLUDED_DIRECTORIES: &[&str] = &["vendor", "target", "node_modules", ".git"];
/// TypeScript discovery stays shallow (workspace packages live near the root).
const TS_EXCLUDED_DIRECTORIES: &[&str] = &["node_modules", ".git", "dist", "target"];
const TS_MAX_DEPTH: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ScipLanguage {
    Rust,
    TypeScript,
    Go,
}

impl ScipLanguage {
    pub fn label(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::TypeScript => "typescript",
            Self::Go => "go",
        }
    }

    fn extensions(self) -> &'static [&'static str] {
        match self {
            Self::Rust => &["rs"],
            Self::TypeScript => &["ts", "tsx", "js", "jsx", "vue"],
            Self::Go => &["go"],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScipPlan {
    pub language: ScipLanguage,
    /// Subdirectory relative to the project root; empty when the plan runs at the root.
    pub subdir: PathBuf,
    pub program: String,
    pub args: Vec<String>,
    /// Absolute path of the index this plan writes.
    pub output: PathBuf,
}

impl ScipPlan {
    /// `CODEGRAPH_SCIP_INDEX` entry: `<output>` at the root, `<output>@<subdir>` otherwise.
    pub fn spec(&self) -> String {
        let output = self.output.to_string_lossy().replace('\\', "/");
        if self.subdir.as_os_str().is_empty() {
            output
        } else {
            format!("{output}@{}", posix(&self.subdir))
        }
    }

    /// Stable cache identity, e.g. `rust:` / `typescript:web` / `go:easytier-go`.
    pub fn cache_key(&self) -> String {
        format!("{}:{}", self.language.label(), posix(&self.subdir))
    }
}

/// Detect the SCIP indexes worth generating for `root`, writing them under `out_dir`.
///
/// The returned order is deterministic: sorted by `(language, subdir)`.
pub fn plan_indexes(root: &Path, out_dir: &Path) -> Vec<ScipPlan> {
    let mut plans = Vec::new();

    if root.join("Cargo.toml").is_file() {
        let output = out_dir.join("rust.scip");
        plans.push(ScipPlan {
            language: ScipLanguage::Rust,
            subdir: PathBuf::new(),
            program: "rust-analyzer".to_string(),
            args: vec![
                "scip".to_string(),
                ".".to_string(),
                "--output".to_string(),
                path_arg(&output),
            ],
            output,
        });
    }

    for dir in find_manifest_dirs(root, "go.mod", usize::MAX, GO_EXCLUDED_DIRECTORIES) {
        let subdir = relative_subdir(root, &dir);
        let output = out_dir.join(format!("go-{}.scip", sanitize(&subdir)));
        plans.push(ScipPlan {
            language: ScipLanguage::Go,
            subdir,
            program: "scip-go".to_string(),
            args: vec!["--output".to_string(), path_arg(&output)],
            output,
        });
    }

    for dir in find_manifest_dirs(root, "package.json", TS_MAX_DEPTH, TS_EXCLUDED_DIRECTORIES) {
        let subdir = relative_subdir(root, &dir);
        let output = out_dir.join(format!("ts-{}.scip", sanitize(&subdir)));
        plans.push(ScipPlan {
            language: ScipLanguage::TypeScript,
            subdir,
            program: "scip-typescript".to_string(),
            args: vec![
                "index".to_string(),
                "--pnpm-workspaces".to_string(),
                "--output".to_string(),
                path_arg(&output),
            ],
            output,
        });
    }

    plans
        .sort_by(|left, right| (left.language, &left.subdir).cmp(&(right.language, &right.subdir)));
    plans
}

/// Fingerprint every source file the plan's language owns under `root.join(subdir)`.
///
/// The value covers `(relative posix path, byte length, mtime seconds)` triples so a
/// touched-but-unchanged file still re-runs the indexer while an unrelated language
/// editing does not invalidate a cached index.
pub fn source_fingerprint(root: &Path, plan: &ScipPlan) -> Result<String> {
    let base = root.join(&plan.subdir);
    let mut files: Vec<(String, u64, u64)> = Vec::new();
    collect_source_files(&base, &base, plan.language.extensions(), &mut files)?;
    files.sort();
    Ok(fingerprint(&files)?)
}

/// Generate (or reuse) every detected index and return `CODEGRAPH_SCIP_INDEX` entries.
///
/// With `strict`, a missing indexer or a failed run is an error instead of skipping
/// that language's compiler-index enrichment.
pub fn ensure_indexes(root: &Path, out_dir: &Path, strict: bool) -> Result<Vec<String>> {
    let plans = plan_indexes(root, out_dir);
    if plans.is_empty() {
        return Ok(Vec::new());
    }

    let cache = ArtifactCache::new(root.join(".codegraph/index-cache"), CACHE_NAMESPACE);
    let mut entries = Vec::with_capacity(plans.len());

    for plan in &plans {
        let current = source_fingerprint(root, plan)?;
        let cache_key = plan.cache_key();

        let cached: Option<CachedIndex> = cache.get(&cache_key);
        if let Some(cached) = cached
            && cached.fingerprint == current
            && cached.output.is_file()
        {
            info!("reusing cached SCIP index for {cache_key}");
            entries.push(plan.spec());
            continue;
        }

        if !program_on_path(&plan.program) {
            if strict {
                bail!(
                    "--scip-strict: required indexer `{}` for {cache_key} was not found on PATH",
                    plan.program
                );
            }
            warn!(
                "SCIP indexer `{}` for {cache_key} was not found on PATH; skipping its compiler-index enrichment",
                plan.program
            );
            continue;
        }

        if let Some(parent) = plan.output.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("cannot create SCIP output directory {}", parent.display())
            })?;
        }

        let output = std::process::Command::new(&plan.program)
            .args(&plan.args)
            .current_dir(root.join(&plan.subdir))
            .output()
            .with_context(|| format!("failed to run `{}` for {cache_key}", plan.program))?;

        if !output.status.success() {
            let tail = stderr_tail(&output.stderr, 20);
            if strict {
                bail!(
                    "`{}` failed while generating the {cache_key} SCIP index ({}):\n{tail}",
                    plan.program,
                    output.status
                );
            }
            warn!(
                "`{}` failed while generating the {cache_key} SCIP index ({}); skipping its compiler-index enrichment:\n{tail}",
                plan.program, output.status
            );
            continue;
        }

        std::fs::create_dir_all(out_dir).with_context(|| {
            format!("cannot create SCIP output directory {}", out_dir.display())
        })?;

        let record = CachedIndex {
            fingerprint: current,
            output: plan.output.clone(),
        };
        if let Err(error) = cache.put(&cache_key, &record) {
            warn!("failed to cache the {cache_key} SCIP index: {error}");
        }

        entries.push(plan.spec());
    }

    Ok(entries)
}

#[derive(Debug, Serialize, Deserialize)]
struct CachedIndex {
    fingerprint: String,
    output: PathBuf,
}

/// Look `program` up on `PATH` without pulling in a `which` dependency.
fn program_on_path(program: &str) -> bool {
    if program.contains('/') || program.contains('\\') {
        return Path::new(program).is_file();
    }
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths)
        .any(|dir| !dir.as_os_str().is_empty() && dir.join(program).is_file())
}

fn collect_source_files(
    base: &Path,
    dir: &Path,
    extensions: &[&str],
    files: &mut Vec<(String, u64, u64)>,
) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("cannot read {}", dir.display()));
        }
    };

    for entry in entries {
        let entry = entry.with_context(|| format!("cannot read {}", dir.display()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(|| format!("cannot stat {}", path.display()))?;

        if file_type.is_dir() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if FINGERPRINT_EXCLUDED_DIRECTORIES.contains(&name.as_ref()) {
                continue;
            }
            collect_source_files(base, &path, extensions, files)?;
            continue;
        }
        if !file_type.is_file() {
            continue;
        }

        let matches_extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extensions.contains(&extension));
        if !matches_extension {
            continue;
        }

        let metadata = entry
            .metadata()
            .with_context(|| format!("cannot stat {}", path.display()))?;
        let relative = path
            .strip_prefix(base)
            .map(posix)
            .unwrap_or_else(|_| posix(&path));
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs())
            .unwrap_or(0);
        files.push((relative, metadata.len(), modified));
    }

    Ok(())
}

fn find_manifest_dirs(
    root: &Path,
    manifest: &str,
    max_depth: usize,
    excluded: &[&str],
) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];

    while let Some((dir, depth)) = stack.pop() {
        if depth > max_depth {
            continue;
        }
        if dir.join(manifest).is_file() {
            found.push(dir.clone());
        }
        if depth == max_depth {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if excluded.contains(&name.as_ref()) {
                continue;
            }
            stack.push((entry.path(), depth + 1));
        }
    }

    found
}

fn relative_subdir(root: &Path, dir: &Path) -> PathBuf {
    dir.strip_prefix(root)
        .map(Path::to_path_buf)
        .unwrap_or_default()
}

/// Posix rendering of a relative path, ignoring `.`/`..`/root components.
fn posix(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn path_arg(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Output file stem: `root` for the project root, `/` replaced with `_` otherwise.
fn sanitize(subdir: &Path) -> String {
    let name = posix(subdir);
    if name.is_empty() {
        "root".to_string()
    } else {
        name.replace('/', "_")
    }
}

fn stderr_tail(stderr: &[u8], lines: usize) -> String {
    let text = String::from_utf8_lossy(stderr);
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(lines);
    all[start..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, relative: &str, contents: &str) {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn detects_rust_from_manifest() {
        let project = tempfile::tempdir().unwrap();
        let root = project.path();
        let out = root.join(".codegraph/scip");
        write(root, "Cargo.toml", "[package]\nname = \"demo\"\n");
        write(root, "src/main.rs", "fn main() {}\n");

        let plans = plan_indexes(root, &out);
        let rust: Vec<_> = plans
            .iter()
            .filter(|plan| plan.language == ScipLanguage::Rust)
            .collect();

        assert_eq!(rust.len(), 1);
        assert_eq!(rust[0].program, "rust-analyzer");
        assert_eq!(rust[0].args[0], "scip");
        assert!(rust[0].subdir.as_os_str().is_empty());
        assert_eq!(rust[0].output, out.join("rust.scip"));
        let spec = rust[0].spec();
        assert_eq!(spec, out.join("rust.scip").to_string_lossy().to_string());
        assert!(!spec.contains('@'));
        assert_eq!(rust[0].cache_key(), "rust:");
    }

    #[test]
    fn detects_typescript_and_marks_subdir() {
        let project = tempfile::tempdir().unwrap();
        let root = project.path();
        let out = root.join(".codegraph/scip");
        write(root, "web/package.json", "{\"name\":\"web\"}\n");
        write(root, "web/src/a.ts", "export const a = 1;\n");

        let plans = plan_indexes(root, &out);
        assert_eq!(plans.len(), 1);
        let plan = &plans[0];

        assert_eq!(plan.language, ScipLanguage::TypeScript);
        assert_eq!(plan.subdir, PathBuf::from("web"));
        assert_eq!(plan.program, "scip-typescript");
        assert_eq!(plan.args[0], "index");
        assert!(plan.args.contains(&"--pnpm-workspaces".to_string()));
        assert!(plan.spec().ends_with("@web"));
        assert_eq!(plan.cache_key(), "typescript:web");
        assert_eq!(plan.output.file_name().unwrap(), "ts-web.scip");
    }

    #[test]
    fn detects_go_module_and_sanitizes_output_name() {
        let project = tempfile::tempdir().unwrap();
        let root = project.path();
        let out = root.join(".codegraph/scip");
        write(
            root,
            "easytier-go/go.mod",
            "module example.com/easytier-go\n",
        );
        write(root, "easytier-go/main.go", "package main\n");

        let plans = plan_indexes(root, &out);
        assert_eq!(plans.len(), 1);
        let plan = &plans[0];

        assert_eq!(plan.language, ScipLanguage::Go);
        assert_eq!(plan.subdir, PathBuf::from("easytier-go"));
        assert_eq!(plan.program, "scip-go");
        assert_eq!(
            plan.args,
            vec!["--output".to_string(), path_arg(&plan.output)]
        );
        assert_eq!(plan.output.file_name().unwrap(), "go-easytier-go.scip");
        assert!(plan.spec().ends_with("@easytier-go"));
        assert_eq!(plan.cache_key(), "go:easytier-go");
    }

    #[test]
    fn reserved_directories_are_ignored() {
        let project = tempfile::tempdir().unwrap();
        let root = project.path();
        let out = root.join(".codegraph/scip");
        write(
            root,
            "node_modules/evil/package.json",
            "{\"name\":\"evil\"}\n",
        );
        write(
            root,
            "target/evil/Cargo.toml",
            "[package]\nname = \"evil\"\n",
        );

        let plans = plan_indexes(root, &out);
        assert!(plans.is_empty());
    }

    #[test]
    fn fingerprint_is_stable_for_unchanged_sources() {
        let project = tempfile::tempdir().unwrap();
        let root = project.path();
        let out = root.join(".codegraph/scip");
        write(root, "Cargo.toml", "[package]\nname = \"demo\"\n");
        write(root, "src/a.rs", "fn a() {}\n");

        let plan = plan_indexes(root, &out)
            .into_iter()
            .find(|plan| plan.language == ScipLanguage::Rust)
            .unwrap();
        let first = source_fingerprint(root, &plan).unwrap();
        let second = source_fingerprint(root, &plan).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn fingerprint_changes_when_content_changes() {
        let project = tempfile::tempdir().unwrap();
        let root = project.path();
        let out = root.join(".codegraph/scip");
        write(root, "Cargo.toml", "[package]\nname = \"demo\"\n");
        write(root, "src/a.rs", "fn a() {}\n");

        let plan = plan_indexes(root, &out)
            .into_iter()
            .find(|plan| plan.language == ScipLanguage::Rust)
            .unwrap();
        let first = source_fingerprint(root, &plan).unwrap();

        write(root, "src/a.rs", "fn a() {}\nfn b() {}\n");
        let second = source_fingerprint(root, &plan).unwrap();
        assert_ne!(first, second);
    }

    #[test]
    fn fingerprint_ignores_files_of_other_languages() {
        let project = tempfile::tempdir().unwrap();
        let root = project.path();
        let out = root.join(".codegraph/scip");
        write(root, "Cargo.toml", "[package]\nname = \"demo\"\n");
        write(root, "src/a.rs", "fn a() {}\n");
        write(root, "src/b.ts", "export const b = 1;\n");

        let plan = plan_indexes(root, &out)
            .into_iter()
            .find(|plan| plan.language == ScipLanguage::Rust)
            .unwrap();
        let first = source_fingerprint(root, &plan).unwrap();

        write(
            root,
            "src/b.ts",
            "export const b = 2;\nexport const c = 3;\n",
        );
        let second = source_fingerprint(root, &plan).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn ordered_plans_are_deterministic() {
        let project = tempfile::tempdir().unwrap();
        let root = project.path();
        let out = root.join(".codegraph/scip");
        write(root, "web/package.json", "{\"name\":\"web\"}\n");
        write(root, "admin/package.json", "{\"name\":\"admin\"}\n");
        write(root, "packages/ui/package.json", "{\"name\":\"ui\"}\n");
        write(
            root,
            "packages/ui/nested/deep/package.json",
            "{\"name\":\"deep\"}\n",
        );

        let sequence = || {
            plan_indexes(root, &out)
                .into_iter()
                .map(|plan| (plan.language, plan.subdir))
                .collect::<Vec<_>>()
        };
        let first = sequence();
        let second = sequence();

        assert_eq!(first, second);
        assert_eq!(
            first,
            vec![
                (ScipLanguage::TypeScript, PathBuf::from("admin")),
                (ScipLanguage::TypeScript, PathBuf::from("packages/ui")),
                (ScipLanguage::TypeScript, PathBuf::from("web")),
            ]
        );
    }
}
