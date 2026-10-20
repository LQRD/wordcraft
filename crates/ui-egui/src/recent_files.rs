//! Desktop Recent availability (#3314). Metadata is read on one worker, never while painting.

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::{Duration, Instant};

const REFRESH: Duration = Duration::from_secs(2);

/// Cached observations of just the recent paths displayed on Home/Open. Unknown paths stay normal.
#[derive(Default)]
pub(crate) struct RecentFiles {
    paths: Vec<String>,
    missing: HashMap<String, bool>,
    pending: Option<Receiver<Vec<(String, bool)>>>,
    next: Option<Instant>,
}

impl RecentFiles {
    /// Refresh at most every two seconds while the list is visible; never overlap workers.
    pub(crate) fn refresh(&mut self, ctx: &egui::Context, paths: &[String], now: Instant) {
        if self.paths != paths {
            self.paths = paths.to_vec();
            self.missing.retain(|path, _| paths.contains(path));
            self.next = None;
        }
        if let Some(rx) = &self.pending {
            match rx.try_recv() {
                Ok(results) => {
                    for (path, missing) in results {
                        if self.paths.contains(&path) {
                            self.missing.insert(path, missing);
                        }
                    }
                    self.pending = None;
                }
                Err(TryRecvError::Disconnected) => self.pending = None,
                Err(TryRecvError::Empty) => {}
            }
        }
        if !paths.is_empty() && self.pending.is_none() && self.next.is_none_or(|next| now >= next) {
            let (tx, rx) = channel();
            let paths = paths.to_vec();
            let wake = ctx.clone();
            self.next = Some(now + REFRESH);
            if std::thread::Builder::new()
                .name("recent-files".into())
                .spawn(move || {
                    let results = paths
                        .into_iter()
                        .map(|path| {
                            let missing = is_missing(std::fs::metadata(&path));
                            (path, missing)
                        })
                        .collect();
                    let _ = tx.send(results);
                    wake.request_repaint();
                })
                .is_ok()
            {
                self.pending = Some(rx);
            }
        }
        if !paths.is_empty() {
            ctx.request_repaint_after(REFRESH);
        }
    }

    /// True only after the OS positively reported a missing path, not for access or I/O errors.
    pub(crate) fn missing(&self, path: &str) -> bool {
        self.missing.get(path).copied().unwrap_or(false)
    }
}

/// Permission denied and unavailable drives do not establish that a file was deleted.
fn is_missing(result: std::io::Result<std::fs::Metadata>) -> bool {
    result.is_err_and(|err| err.kind() == std::io::ErrorKind::NotFound)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #3314: deletion and restoration are observed, and removed recent paths leave the cache.
    #[test]
    fn recent_availability_follows_file_changes() {
        let dir = std::env::temp_dir().join(format!(
            "wordcraft-recent-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("synthetic.docx");
        std::fs::write(&file, b"synthetic").unwrap();
        let paths = vec![file.to_string_lossy().into_owned()];
        let ctx = egui::Context::default();
        let mut state = RecentFiles::default();
        let start = Instant::now();
        let settle = |state: &mut RecentFiles, now| {
            state.refresh(&ctx, &paths, now);
            let deadline = Instant::now() + Duration::from_secs(5);
            while state.pending.is_some() {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(2));
                state.refresh(&ctx, &paths, now);
            }
        };
        settle(&mut state, start);
        assert!(!state.missing(&paths[0]));
        std::fs::remove_file(&file).unwrap();
        settle(&mut state, start + REFRESH);
        assert!(state.missing(&paths[0]));
        std::fs::write(&file, b"restored").unwrap();
        settle(&mut state, start + REFRESH * 2);
        assert!(!state.missing(&paths[0]));
        state.refresh(&ctx, &[], start + REFRESH * 3);
        assert!(state.missing.is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// An inaccessible path must not be labelled as deleted or moved.
    #[test]
    fn recent_access_errors_are_not_missing_files() {
        assert!(is_missing(Err(std::io::ErrorKind::NotFound.into())));
        assert!(!is_missing(Err(std::io::ErrorKind::PermissionDenied.into())));
        assert!(!is_missing(Err(std::io::ErrorKind::TimedOut.into())));
    }

    /// A blocked network probe neither blocks refresh nor starts another worker.
    #[test]
    fn recent_pending_probe_does_not_block_the_ui() {
        let (_tx, rx) = channel();
        let mut state = RecentFiles { pending: Some(rx), ..Default::default() };
        let ctx = egui::Context::default();
        state.refresh(&ctx, &["not-yet-checked.docx".into()], Instant::now());
        assert!(state.pending.is_some());
        assert!(!state.missing("not-yet-checked.docx"));
    }
}
