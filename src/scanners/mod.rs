pub mod movie;
pub mod scanner_daemon;
pub mod tmdb;
pub mod tv_show;

pub use pushevent::EventTx;

use self::scanner_daemon::ScannerDaemon;
use self::tmdb_api::Media;
use self::tmdb_api::MediaType;

use pushevent::Event;

use database::get_conn;
use database::library;
use database::library::Library;
use database::mediafile::InsertableMediaFile;
use database::mediafile::MediaFile;

use crate::streaming::ffprobe::FFProbeCtx;
use crate::streaming::FFPROBE_BIN;

use torrent_name_parser::Metadata;

use slog::debug;
use slog::error;
use slog::info;
use slog::warn;
use slog::Logger;

use walkdir::WalkDir;

use std::fmt;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use serde::Deserialize;
use serde::Serialize;

pub trait APIExec<'a> {
    fn new(api_key: &'a str) -> Self;
    fn search(&mut self, title: String, year: Option<i32>, media_type: MediaType) -> Option<Media>;
    fn search_many(
        &mut self,
        title: String,
        year: Option<i32>,
        media_type: MediaType,
        result_num: usize,
    ) -> Vec<Media>;
    fn search_by_id(&mut self, id: i32, media_type: MediaType) -> Option<Media>;
}

#[derive(Clone, Copy, Debug, Hash, Eq, PartialEq, Serialize, Deserialize)]
pub enum ApiMediaType {
    Movie,
    Tv,
}

impl fmt::Display for ApiMediaType {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ApiMediaType::Movie => write!(f, "movie"),
            ApiMediaType::Tv => write!(f, "tv"),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApiMedia {
    pub id: u64,
    pub title: String,
    pub release_date: Option<String>,
    pub overview: Option<String>,
    pub poster_path: Option<String>,
    pub backdrop_path: Option<String>,
    pub genres: Vec<String>,

    pub media_type: ApiMediaType,
    pub seasons: Vec<ApiSeason>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApiSeason {
    pub id: u64,
    pub name: Option<String>,
    pub poster_path: Option<String>,
    pub season_number: u64,
    pub episodes: Vec<ApiEpisode>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApiEpisode {
    pub id: u64,
    pub name: Option<String>,
    pub overview: Option<String>,
    pub episode: Option<u64>,
    pub still: Option<String>,
}

/// Trait describes the interface a metadata agent must implement.
pub trait MetadataAgent {
    type Error;

    fn search(&mut self, title: String, year: Option<i32>) -> Result<ApiMedia, Self::Error>;
}

#[derive(Debug)]
pub enum ScannerError {
    /// Used when a scanner was invoked over a invalid library type.
    InvalidLibraryType {
        expected: library::MediaType,
        got: library::MediaType,
    },
    /// Used when a internal db error occured.
    InternalDbError,
    /// Filename parser error
    FilenameParserError,
    /// Used when a scanner was started for a non-existant library.
    LibraryDoesntExist(i32),
    /// FFProbe error
    FFProbeError,
    /// Scanner daemon init error
    ScannerDaemonInitError(notify::Error),
    UnknownError,
}

impl From<diesel::ConnectionError> for ScannerError {
    fn from(_: diesel::ConnectionError) -> Self {
        Self::InternalDbError
    }
}

impl From<diesel::result::Error> for ScannerError {
    fn from(_: diesel::result::Error) -> Self {
        Self::InternalDbError
    }
}

impl From<notify::Error> for ScannerError {
    fn from(nresult: notify::Error) -> Self {
        Self::ScannerDaemonInitError(nresult)
    }
}

pub trait MediaScanner: Sized {
    /// The media type that this scanner supports.
    const MEDIA_TYPE: library::MediaType;
    /// The file extensions that this scanner supports.
    const SUPPORTED_EXTS: &'static [&'static str] = &["mkv", "mp4", "avi"];

    /// Function initializes a new scanner.
    ///
    /// # Arguments
    /// `library_id` - the id of the library we want to scan for.
    /// `log` - stdout logger
    /// `event_tx` - event_tx channel over which we can dispatch ws events.
    ///
    /// # Errors
    /// Will return `Err(LibraryDoesntExist(..))` if library with `library_id` doesnt exist.
    /// Will return `Err(InternalDbError)` if we cant acquire a database connection.
    /// Will return `Err(InvalidLibraryType)` if the library with id `library_id` isnt of
    /// media_type `Self::MEDIA_TYPE`.
    fn new(library_id: i32, log: Logger, event_tx: EventTx) -> Result<Self, ScannerError> {
        let conn = get_conn()?;
        let lib = Library::get_one(&conn, library_id)
            .map_err(|_| ScannerError::LibraryDoesntExist(library_id))?;

        if lib.media_type != Self::MEDIA_TYPE {
            return Err(ScannerError::InvalidLibraryType {
                expected: Self::MEDIA_TYPE,
                got: lib.media_type,
            });
        }

        Ok(Self::new_unchecked(conn, lib, log, event_tx))
    }

    /// Function starts listing all the files in the library directory and starts scanning them.
    fn start(&self, custom_path: Option<&str>) {
        let lib = self.library_ref();
        let log = self.logger_ref();
        // sanity check
        debug_assert!(lib.media_type == Self::MEDIA_TYPE);
        info!(
            log,
            "Enumerating files for library={} with media_type={:?}",
            lib.id,
            Self::MEDIA_TYPE
        );

        let path = custom_path.unwrap_or(lib.location.as_str());
        let files: Vec<PathBuf> = WalkDir::new(path)
            // we want to follow all symlinks in case of complex dir structures
            .follow_links(true)
            .into_iter()
            .filter_map(Result::ok)
            // ignore all hidden files.
            .filter(|f| {
                !f.path()
                    .iter()
                    .any(|s| s.to_str().map(|x| x.starts_with('.')).unwrap_or(false))
            })
            // check whether `f` has a supported extension
            .filter(|f| {
                f.path()
                    .extension()
                    .and_then(|e| e.to_str())
                    .map_or(false, |e| Self::SUPPORTED_EXTS.contains(&e))
            })
            .map(|f| f.into_path())
            .collect();

        info!(
            log,
            "Scanning {} files for library {} of {:?}",
            files.len(),
            lib.id,
            Self::MEDIA_TYPE
        );

        // mount the files found into the database.
        // Essentially we extract the bare minimum information from each file such as its codec,
        // title, year and container, and insert it into the database as an orphan media file.
        for file in files {
            if let Err(e) = self.mount_file(file) {
                error!(log, "Failed to mount file into the database: {:?}", e);
            }
        }

        self.fix_orphans();
    }

    // Function parses metadata from file `file` and inserts the data into the database.
    //
    // # Arguments
    // `file` - A pathbuffer containing the path to a media file we are trying to insert into the
    // database. This file will *ALWAYS* have a extension that is in `Self::SUPPORTED_EXTS`.
    fn mount_file(&self, file: PathBuf) -> Result<MediaFile, ScannerError> {
        let conn = self.conn_ref();
        let lib = self.library_ref();
        let log = self.logger_ref();
        let target_file = file.to_str().unwrap();

        let file_name = if let Some(file_name) = file.file_name().and_then(|x| x.to_str()) {
            file_name
        } else {
            error!(
                log,
                "Looks like file={:?} either has a non-unicode file_name, skipping.", file
            );
            return Err(ScannerError::UnknownError);
        };

        if let Ok(media_file) = MediaFile::get_by_file(conn, target_file) {
            debug!(
                log,
                "Tried to mount file that has already been mounted lib_id={} file_path={:?}",
                lib.id,
                file
            );
            return Ok(media_file);
        }

        info!(log, "Scanning file: {} for lib={}", target_file, lib.id);

        let ctx = FFProbeCtx::new(&FFPROBE_BIN);

        // we clone so that we can strip the extension.
        let mut file_name_clone = file.clone();
        file_name_clone.set_extension("");
        // unwrap will never panic because we validate the path earlier on.
        let file_name_clone = file_name_clone.file_name().unwrap().to_str().unwrap();

        let metadata =
            Metadata::from(file_name_clone).map_err(|_| ScannerError::FilenameParserError)?;

        let ffprobe_data = if let Ok(data) = ctx.get_meta(&file) {
            data
        } else {
            error!(log, "Couldnt get data from ffprobe for file={:?}, this could be caused by ffprobe not existing", file);
            return Err(ScannerError::FFProbeError);
        };

        let media_file = InsertableMediaFile {
            media_id: None,
            library_id: lib.id,
            target_file: target_file.to_string(),

            raw_name: metadata.title().to_owned(),
            raw_year: metadata.year(),
            season: metadata.season(),
            episode: metadata.episode(),

            quality: ffprobe_data.get_quality(),
            codec: ffprobe_data.get_codec(),
            container: ffprobe_data.get_container(),
            audio: ffprobe_data.get_audio_type(),
            original_resolution: ffprobe_data.get_res(),
            duration: ffprobe_data.get_duration(),
            corrupt: ffprobe_data.is_corrupt(),
        };

        let file_id = media_file.insert(conn)?;

        Ok(MediaFile::get_one(conn, file_id)?)
    }

    fn fix_orphans(&self);

    /// Function will create a instance of `Self` containing the parameters passed in.
    fn new_unchecked(
        conn: database::DbConnection,
        lib: Library,
        log: Logger,
        event_tx: EventTx,
    ) -> Self;

    fn logger_ref(&self) -> &Logger;
    fn library_ref(&self) -> &Library;
    fn conn_ref(&self) -> &database::DbConnection;
}

pub fn start(library_id: i32, log: &Logger, tx: EventTx) -> Result<(), ()> {
    info!(log, "Summoning scanner for Library with id: {}", library_id);

    if get_conn().is_ok() {
        let log_clone = log.clone();
        let tx_clone = tx.clone();

        let log = log_clone.clone();
        if let Err(e) = scanner_from_library(library_id, log_clone, tx_clone) {
            error!(
                log,
                "Scanner for lib: {} has failed to start with error: {:?}", library_id, e
            )
        }
    } else {
        error!(log, "Failed to connect to db");
        return Err(());
    }

    Ok(())
}

fn scanner_from_library(lib_id: i32, log: Logger, tx: EventTx) -> Result<(), ScannerError> {
    use self::movie::MovieScanner;
    use self::tv_show::TvShowScanner;
    use database::library::MediaType;

    let conn = get_conn()?;
    let library = Library::get_one(&conn, lib_id)?;

    match library.media_type {
        MediaType::Movie => {
            let scanner = MovieScanner::new(lib_id, log, tx)?;
            scanner.start(None);
            scanner.start_daemon()?;
        }
        MediaType::Tv => {
            let scanner = TvShowScanner::new(lib_id, log, tx)?;
            scanner.start(None);
            scanner.start_daemon()?;
        }
        _ => unreachable!(),
    }

    Ok(())
}
