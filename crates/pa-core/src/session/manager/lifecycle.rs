//! The lifecycle concern: the constructors, the open/fork/new/materialize/
//! adopt arm, and the fork's branch-copy helpers.

use super::{
    capture_git_context, create_session_id, format_iso_now, get_session_file_path,
    resolve_session_rlm_depth, root_rlm_depth_from_env, FileEntry, HashMap,
    NewSessionOptions, Path, PathBuf, SessionHeader, SessionManager, CURRENT_SESSION_VERSION,
};

/// The fork's branch copy: drop the source header and its `git_state` rows,
/// re-linking any child whose parent was dropped to the nearest kept
/// ancestor. Re-parented entries round-trip through their own JSON; other
/// fields stay verbatim.
fn forked_branch_entries(entries: Vec<FileEntry>) -> Vec<FileEntry> {
    // git_state rows describe the source repo; the fork reports its own.
    let mut dropped_parent: HashMap<String, Option<String>> = HashMap::new();
    for entry in &entries {
        if matches!(entry, FileEntry::GitState { .. }) {
            if let Some(id) = entry.id() {
                dropped_parent.insert(id.to_string(), entry.parent_id().map(str::to_string));
            }
        }
    }
    // Resolve each dropped id to its nearest kept ancestor lazily (memoized
    // over the ACYCLIC walks only): a cycle's terminal depends on the walk's
    // start, so memoizing one would make the outcome depend on map order.
    let mut resolved: HashMap<String, Option<String>> = HashMap::new();
    entries
        .into_iter()
        .filter(|entry| !matches!(entry, FileEntry::Header { .. } | FileEntry::GitState { .. }))
        .map(|entry| {
            let parent = entry.parent_id().map(str::to_string);
            let live = match &parent {
                // A dropped parent re-links to its resolved kept ancestor
                // (possibly None, re-rooting); a kept parent stays.
                Some(id) if dropped_parent.contains_key(id) => {
                    resolve_dropped_ancestor(&dropped_parent, &mut resolved, id)
                }
                other => other.clone(),
            };
            if entry.parent_id() == live.as_deref() {
                return entry;
            }
            let mut value = serde_json::to_value(&entry).unwrap_or_default();
            if let serde_json::Value::Object(map) = &mut value {
                map.insert(
                    "parentId".to_string(),
                    match &live {
                        Some(id) => serde_json::Value::from(id.clone()),
                        None => serde_json::Value::Null,
                    },
                );
            }
            serde_json::from_value(value).unwrap_or(entry)
        })
        .collect()
}

/// The nearest kept ancestor for one dropped `git_state` row: walk the
/// dropped parents until an id that survives the fork (or a null parent),
/// memoizing the ACYCLIC nodes; a cycle stops at the first repeated id.
fn resolve_dropped_ancestor(
    dropped_parent: &HashMap<String, Option<String>>,
    resolved: &mut HashMap<String, Option<String>>,
    start: &str,
) -> Option<String> {
    let mut path: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut current = Some(start.to_string());
    while let Some(ref id) = current {
        if let Some(answer) = resolved.get(id.as_str()) {
            let answer = answer.clone();
            for node in path {
                resolved.insert(node, answer.clone());
            }
            return answer;
        }
        if !seen.insert(id.clone()) {
            // The first repeated id of THIS walk — memoized for no one else.
            return Some(id.clone());
        }
        if let Some(next) = dropped_parent.get(id.as_str()) {
            path.push(id.clone());
            current.clone_from(next);
        } else {
            let answer = Some(id.clone());
            for node in path {
                resolved.insert(node, answer.clone());
            }
            return answer;
        }
    }
    // The chain ends at a null parent: every node on it re-links to the root.
    for node in path {
        resolved.insert(node, None);
    }
    None
}

impl SessionManager {
    fn uninitialized(cwd: PathBuf, session_dir: PathBuf, persist: bool) -> Self {
        Self {
            session_id: String::new(),
            session_file: None,
            session_dir,
            cwd,
            persist,
            session_dir_backed: persist,
            flushed: false,
            append_ownership: super::window::AppendOwnership::Unleased,
            file_entries: Vec::new(),
            window: None,
            by_id: HashMap::new(),
            labels_by_id: HashMap::new(),
            label_timestamps_by_id: HashMap::new(),
            leaf_id: None,
            admitted_user_rows: std::collections::VecDeque::new(),
            admitted_custom_rows: std::collections::VecDeque::new(),
            persist_listeners: Vec::new(),
            entry_writer: None,
            standalone_log: None,
            read_cursor: crate::session_log::ReadCursor::default(),
        }
    }

    fn new_with(cwd: PathBuf, session_dir: PathBuf, persist: bool) -> Self {
        let mut manager = Self::uninitialized(cwd, session_dir, persist);
        manager.new_session(&NewSessionOptions::default());
        manager
    }

    #[must_use]
    pub fn persisted(cwd: &Path, session_dir: &Path) -> Self {
        Self::new_with(cwd.to_path_buf(), session_dir.to_path_buf(), true)
    }

    #[must_use]
    pub fn in_memory(cwd: &Path) -> Self {
        Self::new_with(cwd.to_path_buf(), cwd.to_path_buf(), false)
    }

    /// Create an in-memory manager pinned to a session's own directory: the
    /// session's identity (its directory, the local harness state's home) stays its own.
    #[must_use]
    pub fn in_memory_in_session_dir(cwd: &Path, session_dir: &Path) -> Self {
        let mut manager = Self::new_with(cwd.to_path_buf(), session_dir.to_path_buf(), false);
        manager.session_dir_backed = true;
        manager
    }

    /// Create a non-persisted mirror of a caller-supplied authoritative header.
    ///
    /// The caller verifies disk authority and owns persistence and any lease.
    /// This constructor does not read or write files, acquire a lease, migrate
    /// metadata, or generate a new identity. `cwd` is the runtime working directory;
    /// the literal `header.cwd` is retained unchanged. `session_dir` has the same
    /// artifact-directory meaning as in [`Self::in_memory_in_session_dir`].
    ///
    /// # Errors
    ///
    /// Returns an error when the supplied header has an empty session id.
    pub fn in_memory_from_header(
        cwd: &Path,
        session_dir: &Path,
        header: SessionHeader,
    ) -> Result<Self, String> {
        if header.id.is_empty() {
            return Err("canonical session header id is empty".into());
        }
        let mut manager = Self::uninitialized(cwd.to_path_buf(), session_dir.to_path_buf(), false);
        manager.session_id.clone_from(&header.id);
        manager.session_dir_backed = true;
        manager.file_entries.push(FileEntry::Header { header });
        Ok(manager)
    }

    /// Whether the manager carries a session directory of its own:
    /// session-owned artifacts (the local harness state) need it.
    #[must_use]
    pub fn has_session_dir(&self) -> bool {
        self.session_dir_backed
    }

    /// Open an existing CRC session log. Unsupported files are never repaired.
    ///
    /// # Errors
    /// Returns validation, lock or filesystem errors without recreating history.
    pub fn open(cwd: &Path, session_dir: &Path, session_file: &Path) -> std::io::Result<Self> {
        let mut manager = Self::uninitialized(cwd.to_owned(), session_dir.to_owned(), true);
        manager.set_session_file(session_file.to_owned(), None)?;
        Ok(manager)
    }

    /// Copy a source session file into a fresh session under `target_cwd`,
    /// parented at the source; the new header carries the source path as
    /// `parentSession`, the resolved RLM depth, and the TARGET cwd's git context.
    ///
    /// # Errors
    ///
    /// Error when the source is not a regular file, is empty or invalid, or
    /// the flush fails.
    pub fn fork_from(
        source_path: &Path,
        target_cwd: &Path,
        session_dir: &Path,
    ) -> Result<Self, String> {
        // A non-regular source (a FIFO or a device) would block the copy's
        // read until a writer appears; a missing path falls through to the
        // empty-or-invalid contract.
        if let Ok(metadata) = std::fs::metadata(source_path) {
            if !metadata.is_file() {
                return Err(format!(
                    "Cannot fork: source session file is not a regular file: {}",
                    source_path.display()
                ));
            }
        }
        // Read-only: repairing would REWRITE the source (dropping a torn row
        // mid-append); the copy skips a torn tail like TS `loadEntriesFromFile`.
        let entries =
            super::super::read::read_entries(source_path).map_err(|error| error.to_string())?;
        if entries.is_empty() {
            return Err(format!(
                "Cannot fork: source session file is empty or invalid: {}",
                source_path.display()
            ));
        }
        let source_header = entries
            .iter()
            .find_map(|entry| match entry {
                FileEntry::Header { header } => Some(header.clone()),
                _ => None,
            })
            .ok_or_else(|| {
                format!(
                    "Cannot fork: source session has no header: {}",
                    source_path.display()
                )
            })?;
        let rlm_depth = resolve_session_rlm_depth(&source_header, source_path);

        let mut forked = Self::persisted(target_cwd, session_dir);
        // A fresh unique id + header: the fork's git context comes from the
        // TARGET cwd, and the source rides along as `parentSession`.
        forked.new_session(&NewSessionOptions {
            id: None,
            parent_session: Some(source_path.display().to_string()),
            rlm_depth: Some(rlm_depth),
        });
        let branch = forked_branch_entries(entries);
        // The copied rows' assistant entries keep the append path durable
        // from the first new entry: one predicate for the durable-append rule.
        forked.adopt_entries(branch);
        forked.flush_now().map_err(|error| error.to_string())?;
        Ok(forked)
    }

    /// Open only the compacted active window off the async executor; use
    /// `active_context` until `ensure_full_history` completes.
    ///
    /// # Errors
    ///
    /// Returns an error when the windowed session store cannot be opened.
    #[cfg(test)]
    pub async fn open_windowed(
        cwd: &Path,
        session_dir: &Path,
        session_file: &Path,
    ) -> anyhow::Result<Self> {
        let cwd = cwd.to_owned();
        let session_dir = session_dir.to_owned();
        let path = session_file.to_owned();
        tokio::task::spawn_blocking(move || {
            let Some(window) = super::window::WindowedSessionStore::open(&path)? else {
                return Self::open(&cwd, &session_dir, &path).map_err(Into::into);
            };
            let mut manager = Self::in_memory(&cwd);
            manager.session_id = match window.entries().first() {
                Some(FileEntry::Header { header }) => header.id.clone(),
                _ => unreachable!("window validates header"),
            };
            manager.session_dir = session_dir;
            manager.session_file = Some(path);
            manager.persist = true;
            manager.session_dir_backed = true;
            // The production adoption path: the test constructor rides the
            // same detach semantics the daemon's engine uses.
            manager.adopt_window(window);
            Ok(manager)
        })
        .await?
    }
    /// Adopt a verified read-only window into an externally persisted
    /// manager: rows go straight to disk, never deferred behind the
    /// bootstrap rule.
    ///
    pub fn adopt_window(&mut self, mut window: super::window::WindowedSessionStore) {
        self.admitted_user_rows.clear();
        self.admitted_custom_rows.clear();
        // One-copy adoption: the parsed trees move in (no `to_vec` clone); the
        // window stays attached for its snapshot/settings/metadata state, and
        // `active_context` walks `file_entries` with the window's settings overlay.
        let (entries, _raw_entries) = window.take_retained();
        self.file_entries = entries;
        self.build_index();
        self.leaf_id = Some(window.leaf_id().to_owned());
        self.flushed = true;
        self.window = Some(window);
    }

    /// Whether this manager's durable appends may certify the window cache
    /// incrementally: only a lease holder may raise it (exactly one writer
    /// per lease); every other manager evicts the live snapshot instead of
    /// extending a certification it cannot guarantee.
    pub fn set_append_ownership(&mut self, ownership: super::window::AppendOwnership) {
        self.append_ownership = ownership;
    }
    /// Switch to a validated existing session log.
    ///
    /// # Errors
    /// Rejects missing or unsupported files; never repairs or rewrites them.
    pub fn set_session_file(
        &mut self,
        path: PathBuf,
        preloaded_entries: Option<Vec<FileEntry>>,
    ) -> std::io::Result<()> {
        // A caller's cached rows cannot establish disk authority.
        let entries = super::super::read::read_entries(&path)?;
        if let Some(preloaded) = preloaded_entries {
            if serde_json::to_value(&preloaded)? != serde_json::to_value(&entries)? {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "preloaded session entries differ from durable log",
                ));
            }
        }
        let Some(FileEntry::Header { header }) = entries.first() else {
            unreachable!("reader validates header")
        };
        self.session_id.clone_from(&header.id);
        self.session_file = Some(path);
        self.window = None;
        self.standalone_log = None;
        self.read_cursor = crate::session_log::ReadCursor::default();
        self.file_entries = entries;
        self.admitted_user_rows.clear();
        self.admitted_custom_rows.clear();
        self.build_index();
        self.flushed = true;
        Ok(())
    }

    /// Create a new session; returns the session file path when persisting.
    ///
    pub fn new_session(&mut self, options: &NewSessionOptions) -> Option<PathBuf> {
        let session_id = options.id.clone().unwrap_or_else(create_session_id);
        let mut session_file: Option<PathBuf> = None;
        if self.persist {
            session_file = Some(get_session_file_path(&self.session_dir, &session_id));
        }

        self.standalone_log = None;
        self.read_cursor = crate::session_log::ReadCursor::default();
        self.session_id = session_id;
        let timestamp = format_iso_now();
        let git = None;
        let rlm_depth = match options.rlm_depth {
            Some(depth) => Some(depth),
            None => options
                .parent_session
                .as_deref()
                .map(|_| 0)
                .or(Some(root_rlm_depth_from_env())),
        };
        let header = FileEntry::Header {
            header: SessionHeader {
                id: self.session_id.clone(),
                version: Some(CURRENT_SESSION_VERSION),
                timestamp,
                cwd: self.cwd.display().to_string(),
                parent_session: options.parent_session.clone(),
                rlm_depth,
                git,
                rest: pa_types::JsonMap::new(),
            },
        };
        self.file_entries = vec![header];
        self.admitted_user_rows.clear();
        self.admitted_custom_rows.clear();
        self.window = None;
        self.by_id.clear();
        self.labels_by_id.clear();
        self.label_timestamps_by_id.clear();
        self.leaf_id = None;
        self.flushed = false;
        if self.persist {
            self.session_file.clone_from(&session_file);
        }
        session_file
    }
    /// Materialize an in-memory session into a persisted file.
    ///
    /// # Panics
    ///
    /// Asserts that the manager holds no windowed store: hydrate the full session history first.
    pub fn materialize_session_file(
        &mut self,
        session_dir: Option<PathBuf>,
    ) -> std::io::Result<PathBuf> {
        assert!(
            self.window.is_none(),
            "hydrate full session history before this operation"
        );
        if let Some(session_file) = self.session_file.clone() {
            self.flush_now()?;
            return Ok(session_file);
        }
        let dir = session_dir
            .or_else(|| {
                (!self.session_dir.as_os_str().is_empty()).then(|| self.session_dir.clone())
            })
            .unwrap_or_else(|| self.cwd.join("sessions"));
        std::fs::create_dir_all(&dir)?;
        let previous_header = self.get_header().cloned();
        let session_id = create_session_id();
        let target = get_session_file_path(&dir, &session_id);
        self.session_dir = dir;
        self.session_id.clone_from(&session_id);
        self.session_file = Some(target.clone());
        self.persist = true;
        self.session_dir_backed = true;
        let timestamp = format_iso_now();
        let git = capture_git_context(&self.cwd);
        let header = FileEntry::Header {
            header: SessionHeader {
                id: session_id,
                version: Some(CURRENT_SESSION_VERSION),
                timestamp,
                cwd: self.cwd.display().to_string(),
                parent_session: previous_header
                    .as_ref()
                    .and_then(|header| header.parent_session.clone()),
                rlm_depth: Some(
                    previous_header
                        .as_ref()
                        .and_then(|header| header.rlm_depth)
                        .unwrap_or(0),
                ),
                git,
                rest: pa_types::JsonMap::new(),
            },
        };
        let rest = std::mem::take(&mut self.file_entries);
        self.file_entries = vec![header];
        self.file_entries.extend(
            rest.into_iter()
                .filter(|entry| !matches!(entry, FileEntry::Header { .. })),
        );
        self.standalone_log = None;
        self.read_cursor = crate::session_log::ReadCursor::default();
        self.flush_now()?;
        Ok(target)
    }
    /// Adopt a durable branch as this session's entries: keeps the header,
    /// replaces every entry with the given chain, and re-indexes so the leaf
    /// is the last adopted entry. In-memory only — the caller owns persistence.
    ///
    pub fn adopt_entries(&mut self, entries: Vec<FileEntry>) {
        self.admitted_user_rows.clear();
        self.admitted_custom_rows.clear();
        // The caller supplies the complete selected branch after explicit navigation.
        self.window = None;
        let header = self
            .file_entries
            .iter()
            .position(|entry| matches!(entry, FileEntry::Header { .. }));
        match header {
            Some(index) => {
                self.file_entries.truncate(index + 1);
                self.file_entries.extend(entries);
            }
            None => {
                self.file_entries = entries;
            }
        }
        self.build_index();
    }
}
