use std::{path, time};

use crate::app::{deltas, sessions};

pub enum Event {
    Tick(time::SystemTime),
    Flush(sessions::Session),
    SessionFlushed(sessions::Session),
    Fetch,

    FileChange(path::PathBuf),
    GitFileChange(path::PathBuf),
    GitIndexChange,
    GitActivity,
    GitHeadChange(String),

    ProjectFileChange(path::PathBuf),

    Session(sessions::Session),
    File((sessions::Session, path::PathBuf, String)),
    Deltas((sessions::Session, path::PathBuf, Vec<deltas::Delta>)),
}
