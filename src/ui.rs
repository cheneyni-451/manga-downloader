use dialoguer::{Select, console::Style, theme::ColorfulTheme};
use log::debug;

use crate::{errors::ScraperErrors, models::Chapter};

pub fn select_chapter_range(mut chapters: Vec<Chapter>) -> anyhow::Result<Vec<Chapter>> {
    let selection_theme = ColorfulTheme {
        prompt_style: Style::default().blue(),
        active_item_style: Style::default().reverse(),
        ..Default::default()
    };
    let selection_start = Select::with_theme(&selection_theme)
        .with_prompt("Select the first chapter to download")
        .items(&chapters)
        .max_length(10)
        .interact_opt()?;
    match selection_start {
        Some(start) => {
            debug!("selected chapter range start index: {start}");
            chapters.drain(..start);
        }
        None => return Err(ScraperErrors::InvalidChapterSelection.into()),
    };
    let selection_end = Select::with_theme(&selection_theme)
        .with_prompt("Select the last chapter to download")
        .items(&chapters)
        .max_length(10)
        .interact_opt()?;
    match selection_end {
        Some(end) => {
            debug!("selected chapter range end index: {end}");
            chapters.truncate(end.saturating_add(1));
            Ok(chapters)
        }
        None => Err(ScraperErrors::InvalidChapterSelection.into()),
    }
}
