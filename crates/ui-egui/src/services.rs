pub type ReadFn = Box<dyn Fn(&str) -> Result<Vec<u8>, String>>;

pub type WriteFn = Box<dyn FnMut(&str, &[u8]) -> Result<(), String>>;

pub type PickFn = Box<dyn FnMut(&str) -> Option<String>>;

/// `(purpose, request id)`. The host shows a picker and later pushes an [`InboxEvent`]
/// carrying that same id. Completing a cancelled or unknown id does nothing: the file is
/// not applied to whichever document happens to be active.
pub type OpenAsyncFn = Box<dyn FnMut(&str, u64)>;

pub type DownloadFn = Box<dyn FnMut(&str, &[u8])>;

/// A file handed to the host page ([`Services::host_save`]), described when its bytes were made.
pub struct HostSave<'a> {
    pub name: &'a str,
    pub bytes: &'a [u8],
    /// The command asked for a new file (Save As, Export, Print): the page never writes it over
    /// the presentation's own file. False for Save.
    pub save_as: bool,
    /// The presentation it came from ([`deckcraft_engine::DocState::uid`]).
    pub document: u64,
    /// A Save or Save As of the presentation itself: its content as saved, which a host that
    /// confirms saves marks saved ([`Services::host_confirms_saves`]). `None` for copies (Export,
    /// Print).
    pub saved: Option<std::sync::Arc<deckcraft_engine::model::Presentation>>,
}

/// [`Services::host_save`]. An error means the page didn't get the file, and the save failed.
pub type HostSaveFn = Box<dyn FnMut(&HostSave<'_>) -> Result<(), String>>;

/// The presentation a file handed to [`SlideApp::open_or_insert_bytes`] opened as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpenedFile {
    /// Its [`deckcraft_engine::DocState::uid`].
    pub document: u64,
    /// Saving it over the file keeps what the file had: a native or PowerPoint package read
    /// whole. False for a repaired copy of a damaged file and for a file read as an outline;
    /// those must go to a new file.
    pub in_place: bool,
}

/// A file the host finished reading for the request `id` from [`SlideApp::begin_external_file`].
#[derive(Clone, Debug)]
pub struct InboxFile {
    /// Cancellation identity. A file whose id was cancelled, or never started, is ignored.
    pub id: u64,
    pub name: String,
    pub bytes: Vec<u8>,
}

/// What the host delivers through [`Inbox`] after an asynchronous read.
#[derive(Clone, Debug)]
pub enum InboxEvent {
    File(InboxFile),
    /// The picker was dismissed or the file could not be read. `id` will not be applied.
    Cancelled {
        id: u64,
    },
}

/// Files delivered asynchronously by the host (web file picker, dropped files).
pub type Inbox = std::sync::Arc<std::sync::Mutex<Vec<InboxEvent>>>;

/// Platform services injected by the host (desktop or web).
#[derive(Default)]
pub struct Services {
    /// Browser-owned presenter popup; absent on native hosts.
    pub presenter: Option<Box<dyn crate::presenter::Host>>,
    /// Open-file dialog for a purpose (`open`, `picture`, `audio`, `video`) → path.
    pub pick_open: Option<PickFn>,
    /// Save dialog with a suggested name → path.
    pub pick_save: Option<PickFn>,
    pub read: Option<ReadFn>,
    pub write: Option<WriteFn>,
    /// Asynchronous open dialog (web). The callback receives the purpose and the request id
    /// from [`SlideApp::begin_external_file`]; the chosen file arrives through `inbox`.
    pub open_async: Option<OpenAsyncFn>,
    /// Hand bytes to the user as a named file (browser download).
    pub download: Option<DownloadFn>,
    /// Web, framed by a page that keeps the files (`?host=parent`): Save, Save As, Export and
    /// Print hand their file to the page here instead of `download`, with the presentation it
    /// came from and whether it asked for a new file ([`HostSave`]). A Save or Save As then
    /// counts as saving the presentation, unless `host_confirms_saves`.
    pub host_save: Option<HostSaveFn>,
    /// With `host_save`: the page says when it has stored a save, and only then does the
    /// presentation count as saved (the host marks it). Until then the Save Changes prompt's
    /// Save doesn't close the presentation.
    pub host_confirms_saves: bool,
    pub inbox: Option<Inbox>,
    /// Image on the system clipboard (PNG bytes), when the host can read one.
    pub clipboard_image: Option<Box<dyn FnMut() -> Option<Vec<u8>>>>,
    /// HTML on the system clipboard (Pages, Keynote, and other apps that write `public.html`).
    pub clipboard_html: Option<Box<dyn FnMut() -> Option<String>>>,
    /// Put HTML and its plain-text alternative on the system clipboard. `true` when it landed.
    pub clipboard_set_html: Option<Box<dyn FnMut(&str, &str) -> bool>>,
    /// Audio output for media playback (cpal on desktop); without one media plays silently.
    pub audio_out: Option<Box<dyn deckcraft_media::AudioOut>>,
}
