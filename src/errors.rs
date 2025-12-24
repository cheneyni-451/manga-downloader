use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ScraperErrors {
    #[error("failed to download {chapter_path} page {page_num}")]
    PageDownloadFailed {
        url: String,
        chapter_path: PathBuf,
        page_num: usize,
    },

    #[error("failed to get title for id: {0}")]
    InvalidBookId(usize),

    #[error("incomplete chapter selection")]
    InvalidChapterSelection,
}
