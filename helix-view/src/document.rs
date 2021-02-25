use anyhow::Error;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use helix_core::{
    syntax::LOADER, ChangeSet, Diagnostic, History, Rope, Selection, State, Syntax, Transaction,
};

#[derive(Copy, Clone, PartialEq, Eq, Hash)]
pub enum Mode {
    Normal,
    Insert,
    Goto,
}

pub struct Document {
    pub state: State, // rope + selection
    /// File path on disk.
    path: Option<PathBuf>,

    /// Current editing mode.
    pub mode: Mode,
    pub restore_cursor: bool,

    /// Tree-sitter AST tree
    pub syntax: Option<Syntax>,
    /// Corresponding language scope name. Usually `source.<lang>`.
    language: Option<String>,

    /// Pending changes since last history commit.
    changes: ChangeSet,
    /// State at last commit. Used for calculating reverts.
    old_state: Option<State>,
    /// Undo tree.
    history: History,
    /// Current document version, incremented at each change.
    version: i32, // should be usize?

    pub diagnostics: Vec<Diagnostic>,
    pub language_server: Option<Arc<helix_lsp::Client>>,
}

/// Like std::mem::replace() except it allows the replacement value to be mapped from the
/// original value.
fn take_with<T, F>(mut_ref: &mut T, closure: F)
where
    F: FnOnce(T) -> T,
{
    use std::{panic, ptr};

    unsafe {
        let old_t = ptr::read(mut_ref);
        let new_t = panic::catch_unwind(panic::AssertUnwindSafe(|| closure(old_t)))
            .unwrap_or_else(|_| ::std::process::abort());
        ptr::write(mut_ref, new_t);
    }
}

use helix_lsp::lsp;
use url::Url;

impl Document {
    pub fn new(state: State) -> Self {
        let changes = ChangeSet::new(&state.doc);
        let old_state = None;

        Self {
            path: None,
            state,
            mode: Mode::Normal,
            restore_cursor: false,
            syntax: None,
            language: None,
            changes,
            old_state,
            diagnostics: Vec::new(),
            version: 0,
            history: History::default(),
            language_server: None,
        }
    }

    // TODO: passing scopes here is awkward
    // TODO: async fn?
    pub fn load(path: PathBuf, scopes: &[String]) -> Result<Self, Error> {
        use std::{env, fs::File, io::BufReader};
        let _current_dir = env::current_dir()?;

        let doc = Rope::from_reader(BufReader::new(File::open(path.clone())?))?;

        // TODO: create if not found

        let mut doc = Self::new(State::new(doc));

        let language_config = LOADER.language_config_for_file_name(path.as_path());
        doc.set_language(language_config, scopes);

        // canonicalize path to absolute value
        doc.path = Some(std::fs::canonicalize(path)?);

        Ok(doc)
    }

    // TODO: do we need some way of ensuring two save operations on the same doc can't run at once?
    // or is that handled by the OS/async layer
    pub fn save(&self) -> impl Future<Output = Result<(), anyhow::Error>> {
        // we clone and move text + path into the future so that we asynchronously save the current
        // state without blocking any further edits.

        let text = self.text().clone();
        let path = self.path.clone().expect("Can't save with no path set!"); // TODO: handle no path

        // TODO: mark changes up to now as saved
        // TODO: mark dirty false

        async move {
            use smol::{fs::File, prelude::*};
            let mut file = File::create(path).await?;

            // write all the rope chunks to file
            for chunk in text.chunks() {
                file.write_all(chunk.as_bytes()).await?;
            }
            // TODO: flush?

            Ok(())
        } // and_then notify save
    }

    pub fn set_language(
        &mut self,
        language_config: Option<Arc<helix_core::syntax::LanguageConfiguration>>,
        scopes: &[String],
    ) {
        if let Some(language_config) = language_config {
            // TODO: maybe just keep an Arc<> pointer to the language_config?
            self.language = Some(language_config.scope().to_string());

            // TODO: this ties lsp support to tree-sitter enabled languages for now. Language
            // config should use Option<HighlightConfig> to let us have non-tree-sitter configs.

            let highlight_config = language_config
                .highlight_config(scopes)
                .expect("No highlight_config found!");
            // TODO: config.configure(scopes) is now delayed, is that ok?

            let syntax = Syntax::new(&self.state.doc, highlight_config);

            self.syntax = Some(syntax);
        } else {
            self.syntax = None;
            self.language = None;
        };
    }

    pub fn set_language2(&mut self, scope: &str, scopes: &[String]) {
        let language_config = LOADER.language_config_for_scope(scope);
        self.set_language(language_config, scopes);
    }

    pub fn set_language_server(&mut self, language_server: Option<Arc<helix_lsp::Client>>) {
        self.language_server = language_server;
    }

    pub fn set_selection(&mut self, selection: Selection) {
        // TODO: use a transaction?
        self.state.selection = selection;
    }

    pub fn _apply(&mut self, transaction: &Transaction) -> bool {
        let old_doc = self.text().clone();

        let success = transaction.apply(&mut self.state);

        if !transaction.changes().is_empty() {
            // TODO: self.version += 1;?

            // update tree-sitter syntax tree
            if let Some(syntax) = &mut self.syntax {
                // TODO: no unwrap
                syntax
                    .update(&old_doc, &self.state.doc, transaction.changes())
                    .unwrap();
            }

            // TODO: map state.diagnostics over changes::map_pos too

            // emit lsp notification
            if let Some(language_server) = &self.language_server {
                let notify = language_server.text_document_did_change(
                    self.versioned_identifier(),
                    &old_doc,
                    self.text(),
                    transaction.changes(),
                );

                smol::block_on(notify).expect("failed to emit textDocument/didChange");
            }
        }
        success
    }

    pub fn apply(&mut self, transaction: &Transaction) -> bool {
        // store the state just before any changes are made. This allows us to undo to the
        // state just before a transaction was applied.
        if self.changes.is_empty() && !transaction.changes().is_empty() {
            self.old_state = Some(self.state.clone());
        }

        let success = self._apply(&transaction);

        if !transaction.changes().is_empty() {
            // Compose this transaction with the previous one
            take_with(&mut self.changes, |changes| {
                changes.compose(transaction.changes().clone())
            });
        }
        success
    }

    pub fn undo(&mut self) -> bool {
        if let Some(transaction) = self.history.undo() {
            self.version += 1;
            let success = self._apply(&transaction);

            // reset changeset to fix len
            self.changes = ChangeSet::new(self.text());

            return success;
        }
        false
    }

    pub fn redo(&mut self) -> bool {
        if let Some(transaction) = self.history.redo() {
            self.version += 1;

            let success = self._apply(&transaction);

            // reset changeset to fix len
            self.changes = ChangeSet::new(self.text());

            return success;
        }
        false
    }

    pub fn append_changes_to_history(&mut self) {
        if self.changes.is_empty() {
            return;
        }

        // TODO: change -> change -> undo -> change -> change fails, probably old_state needs reset

        let new_changeset = ChangeSet::new(self.text());
        let changes = std::mem::replace(&mut self.changes, new_changeset);
        // Instead of doing this messy merge we could always commit, and based on transaction
        // annotations either add a new layer or compose into the previous one.
        let transaction = Transaction::from(changes).with_selection(self.selection().clone());

        // increment document version
        self.version += 1;

        // HAXX: we need to reconstruct the state as it was before the changes..
        let old_state = self.old_state.take().expect("no old_state available");

        self.history.commit_revision(&transaction, &old_state);
    }

    #[inline]
    pub fn mode(&self) -> Mode {
        self.mode
    }

    #[inline]
    /// Corresponding language scope name. Usually `source.<lang>`.
    pub fn language(&self) -> Option<&str> {
        self.language.as_deref()
    }

    #[inline]
    pub fn version(&self) -> i32 {
        self.version
    }

    #[inline]
    pub fn path(&self) -> Option<&PathBuf> {
        self.path.as_ref()
    }

    pub fn url(&self) -> Option<Url> {
        self.path().map(|path| Url::from_file_path(path).unwrap())
    }

    pub fn text(&self) -> &Rope {
        &self.state.doc
    }

    pub fn selection(&self) -> &Selection {
        &self.state.selection
    }

    pub fn relative_path(&self) -> Option<&Path> {
        let cwdir = std::env::current_dir().expect("couldn't determine current directory");

        self.path
            .as_ref()
            .map(|path| path.strip_prefix(cwdir).unwrap_or(path))
    }

    // pub fn slice<R>(&self, range: R) -> RopeSlice where R: RangeBounds {
    //     self.state.doc.slice
    // }

    // TODO: transact(Fn) ?

    // -- LSP methods

    pub fn identifier(&self) -> lsp::TextDocumentIdentifier {
        lsp::TextDocumentIdentifier::new(self.url().unwrap())
    }

    pub fn versioned_identifier(&self) -> lsp::VersionedTextDocumentIdentifier {
        lsp::VersionedTextDocumentIdentifier::new(self.url().unwrap(), self.version)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn changeset_to_changes() {
        use helix_core::{Rope, State, Transaction};
        // use helix_view::Document;
        use helix_lsp::{lsp, Client};
        let text = Rope::from("hello");
        let mut state = State::new(text);
        state.selection = Selection::single(5, 5);
        let mut doc = Document::new(state);

        // insert

        let transaction = Transaction::insert(&doc.state, " world".into());
        let old_doc = doc.state.clone();
        doc.apply(&transaction);
        let changes = Client::changeset_to_changes(&old_doc.doc, doc.text(), transaction.changes());

        assert_eq!(
            changes,
            &[lsp::TextDocumentContentChangeEvent {
                range: Some(lsp::Range::new(
                    lsp::Position::new(0, 5),
                    lsp::Position::new(0, 5)
                )),
                text: " world".into(),
                range_length: None,
            }]
        );

        // delete

        let transaction = transaction.invert(&old_doc);
        let old_doc = doc.state.clone();
        doc.apply(&transaction);
        let changes = Client::changeset_to_changes(&old_doc.doc, doc.text(), transaction.changes());

        // line: 0-based.
        // col: 0-based, gaps between chars.
        // 0 1 2 3 4 5 6 7 8 9 0 1
        // |h|e|l|l|o| |w|o|r|l|d|
        //           -------------
        // (0, 5)-(0, 11)
        assert_eq!(
            changes,
            &[lsp::TextDocumentContentChangeEvent {
                range: Some(lsp::Range::new(
                    lsp::Position::new(0, 5),
                    lsp::Position::new(0, 11)
                )),
                text: "".into(),
                range_length: None,
            }]
        );

        // replace

        // also tests that changes are layered, positions depend on previous changes.

        doc.state.selection = Selection::single(0, 5);
        let transaction = Transaction::change(
            &doc.state,
            vec![(0, 2, Some("aei".into())), (3, 5, Some("ou".into()))].into_iter(),
        );
        // aeilou
        doc.apply(&transaction);
        let changes =
            Client::changeset_to_changes(&doc.state.doc, doc.text(), transaction.changes());

        assert_eq!(
            changes,
            &[
                // 0 1 2 3 4 5
                // |h|e|l|l|o|
                // ----
                //
                // aeillo
                lsp::TextDocumentContentChangeEvent {
                    range: Some(lsp::Range::new(
                        lsp::Position::new(0, 0),
                        lsp::Position::new(0, 2)
                    )),
                    text: "aei".into(),
                    range_length: None,
                },
                // 0 1 2 3 4 5 6
                // |a|e|i|l|l|o|
                //         -----
                //
                // aeilou
                lsp::TextDocumentContentChangeEvent {
                    range: Some(lsp::Range::new(
                        lsp::Position::new(0, 4),
                        lsp::Position::new(0, 6)
                    )),
                    text: "ou".into(),
                    range_length: None,
                }
            ]
        );
    }
}