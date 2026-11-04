// ABOUTME: Verifies ~/.codegraph.env acts as a fallback behind the project .env.
// ABOUTME: Isolates HOME and the working directory so no real user configuration is read.

use std::fs;
use std::path::{Path, PathBuf};

use codegraph_core::config_manager::ConfigManager;

const KEYS: [&str; 3] = [
    "CG_TEST_GLOBAL_ONLY",
    "CG_TEST_SHARED",
    "CG_TEST_PROJECT_ONLY",
];

struct EnvGuard {
    original_dir: PathBuf,
    original_home: Option<std::ffi::OsString>,
}

impl EnvGuard {
    fn enter(home: &Path, project: &Path) -> Self {
        let original_dir = std::env::current_dir().expect("cwd");
        let original_home = std::env::var_os("HOME");
        for key in KEYS {
            unsafe { std::env::remove_var(key) };
        }
        unsafe { std::env::set_var("HOME", home) };
        std::env::set_current_dir(project).expect("chdir into project");
        Self {
            original_dir,
            original_home,
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for key in KEYS {
            unsafe { std::env::remove_var(key) };
        }
        let _ = std::env::set_current_dir(&self.original_dir);
        match self.original_home.take() {
            Some(home) => unsafe { std::env::set_var("HOME", home) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }
}

#[test]
fn project_env_overrides_global_and_global_fills_gaps() {
    let root = tempfile::tempdir().expect("tempdir");
    let home = root.path().join("home");
    let project = root.path().join("project");
    fs::create_dir_all(&home).expect("home dir");
    fs::create_dir_all(&project).expect("project dir");

    fs::write(
        home.join(".codegraph.env"),
        "CG_TEST_GLOBAL_ONLY=global\nCG_TEST_SHARED=global\nCG_TEST_PROJECT_ONLY=global\n",
    )
    .expect("write user env");
    fs::write(
        project.join(".env"),
        "CG_TEST_SHARED=project\nCG_TEST_PROJECT_ONLY=project\n",
    )
    .expect("write project env");

    let _guard = EnvGuard::enter(&home, &project);

    unsafe { ConfigManager::initialize_environment() };

    // The project file wins for keys it defines...
    assert_eq!(
        std::env::var("CG_TEST_SHARED").ok().as_deref(),
        Some("project")
    );
    assert_eq!(
        std::env::var("CG_TEST_PROJECT_ONLY").ok().as_deref(),
        Some("project")
    );
    // ...and keys it omits still come from the user-level fallback.
    assert_eq!(
        std::env::var("CG_TEST_GLOBAL_ONLY").ok().as_deref(),
        Some("global"),
        "the user-level ~/.codegraph.env must fill in keys the project omits"
    );
}
