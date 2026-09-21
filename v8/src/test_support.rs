use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT_WORKSPACE_ID: AtomicU64 = AtomicU64::new(0);

pub(crate) struct TestWorkspace {
    path: PathBuf,
}
impl TestWorkspace {
    pub(crate) fn new(label: &str) -> Self {
        let id = NEXT_WORKSPACE_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "mini-harness-v8-{label}-{}-{id}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self { path }
    }
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}
impl Drop for TestWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
