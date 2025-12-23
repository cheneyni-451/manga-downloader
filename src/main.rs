use std::{
    fmt::Display,
    path::{Path, PathBuf},
    time::Duration,
};

use clap::Parser;
use dialoguer::{Select, console::Style, theme::ColorfulTheme};
use futures::stream::StreamExt;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use reqwest::{
    Client, Url,
    header::{self, HeaderMap, HeaderValue},
};
use scraper::Selector;
use tokio::{fs, io::AsyncWriteExt};

use crate::errors::ScraperErrors;

mod errors;

async fn download_file(
    client: &Client,
    url: &str,
    chapter_path: &Path,
    page_num: usize,
    progress_bar: &ProgressBar,
) -> anyhow::Result<()> {
    let fetch_image = async move || client.get(url).send().await?.bytes().await;
    match fetch_image().await {
        Ok(data) => {
            let file_path = chapter_path.join(format!("{page_num:03}.jpg"));
            let mut downloaded_file = fs::File::create(file_path).await?;
            downloaded_file.write_all(&data).await?;
            progress_bar.tick();

            Ok(())
        }
        Err(_) => Err(ScraperErrors::PageDownloadFailed {
            url: url.to_string(),
            chapter_path: chapter_path.to_path_buf(),
            page_num,
        }
        .into()),
    }
}

async fn download_chapter(
    client: &Client,
    chapter_url: &str,
    chapter_path: &Path,
    progress_bar: &ProgressBar,
) -> anyhow::Result<Vec<usize>> {
    async fn fetch_image_urls(client: &Client, chapter_url: &str) -> anyhow::Result<Vec<String>> {
        let html_content = client.get(chapter_url).send().await?.text().await?;

        let doc = scraper::Html::parse_document(&html_content);

        let selector = Selector::parse("div>chapter-page img").unwrap();
        let images = doc.select(&selector);
        Ok(images
            .map(|img| {
                img.attr("src")
                    .unwrap_or_else(|| img.attr("data-src").unwrap_or_default())
                    .to_string()
            })
            .collect::<Vec<String>>())
    }
    let image_urls = fetch_image_urls(client, chapter_url).await?;

    let tasks = futures::stream::iter(image_urls)
        .enumerate()
        .map(|(page_num, page_url)| async move {
            download_file(client, &page_url, chapter_path, page_num, progress_bar).await
        })
        .buffer_unordered(10);

    let results = tasks.collect::<Vec<_>>().await;
    let failed_pages = results
        .into_iter()
        .filter_map(|result| -> Option<usize> {
            match result {
                Err(err) => {
                    if let Ok(ScraperErrors::PageDownloadFailed { page_num, .. }) = err.downcast() {
                        Some(page_num)
                    } else {
                        None
                    }
                }
                Ok(_) => None,
            }
        })
        .collect();

    Ok(failed_pages)
}

#[derive(Debug, Clone)]
struct Chapter {
    url: String,
    title: String,
}

impl Display for Chapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.title)
    }
}

async fn fetch_chapters_urls(client: &Client, title_url: &str) -> Vec<Chapter> {
    match client.get(title_url).send().await {
        Ok(response) => {
            let response_text = response.text().await;
            if response_text.is_err() {
                return vec![];
            }

            let html_content = response_text.unwrap();
            let doc = scraper::Html::parse_document(&html_content);

            let selector = Selector::parse("#chapters a").unwrap();
            doc.select(&selector)
                .map(|a| {
                    let mut title = a
                        .attr("title")
                        .unwrap_or_default()
                        .trim()
                        .to_ascii_lowercase()
                        .to_string();
                    title = title.replace('/', "-");
                    let chapter_num_pos = title.rfind(char::is_whitespace).unwrap_or_default();
                    let chapter_num_str = title.split_off(chapter_num_pos + 1);
                    let number_width = if let Some(i) = chapter_num_str.rfind('.') {
                        chapter_num_str.len() - i + 4
                    } else {
                        4
                    };

                    Chapter {
                        url: a.attr("href").unwrap_or_default().to_string(),
                        title: format!("{title}{chapter_num_str:0>number_width$}"),
                    }
                })
                .rev()
                .collect()
        }
        Err(_) => {
            vec![]
        }
    }
}

fn split_jobs<T: Clone>(jobs: &mut Vec<T>, num_batches: usize) -> Vec<Vec<T>> {
    let n = jobs.len();

    let chunk_size = n / num_batches;

    let mut batches = Vec::with_capacity(num_batches);
    for _ in 0..(n % num_batches) {
        let batch = jobs.split_off(jobs.len() - (chunk_size + 1));
        batches.push(batch);
    }
    while !jobs.is_empty() {
        batches.push(jobs.split_off(jobs.len().saturating_sub(chunk_size)));
    }

    batches
}

async fn get_title_from_id(client: &Client, id: usize) -> anyhow::Result<(String, Url)> {
    let response = client.get(format!("{HOST_URL}/manga/{id}")).send().await?;
    let url = response.url();
    match url.path_segments() {
        Some(mut segments) => {
            let title = segments.next_back().unwrap();
            Ok((title.to_string(), url.clone()))
        }
        None => Err(ScraperErrors::InvalidBookId(id).into()),
    }
}

fn select_chapters(mut chapters: Vec<Chapter>) -> Vec<Chapter> {
    let selection_theme = ColorfulTheme {
        prompt_style: Style::default().blue(),
        active_item_style: Style::default().reverse(),
        ..Default::default()
    };
    let chapter_selection_start = Select::with_theme(&selection_theme)
        .with_prompt("First of the chapters to download")
        .items(&chapters)
        .max_length(10)
        .interact()
        .unwrap();
    chapters.drain(0..chapter_selection_start);
    let chapter_selection_end = Select::with_theme(&selection_theme)
        .with_prompt("Last of the chapters to download")
        .items(&chapters)
        .max_length(10)
        .interact()
        .unwrap();
    chapters.truncate(chapter_selection_end + 1);

    chapters
}

async fn download_chapters(
    client: &Client,
    chapters: &Vec<Chapter>,
    manga_path: &Path,
    chapter_progress: &ProgressBar,
    total_progress: &ProgressBar,
) {
    chapter_progress.tick();
    for Chapter {
        url,
        title: chapter_title,
    } in chapters
    {
        chapter_progress.set_message(format!("downloading {chapter_title}"));

        let chapter_url = format!("{HOST_URL}{url}");
        match download_chapter(
            client,
            &chapter_url,
            &manga_path.join(chapter_title),
            chapter_progress,
        )
        .await
        {
            Ok(_failed_pages) => {}
            Err(err) => {
                eprintln!("{err}")
            }
        }

        chapter_progress.inc(1);
        total_progress.inc(1);
    }

    chapter_progress.finish();
}

#[derive(Parser, Debug, Clone)]
struct Args {
    #[arg(
        required = true,
        help = "ID of manga in the URL: mangapill.com/manga/<ID>/<TITLE>"
    )]
    id: usize,

    #[arg(short = 'j', long, default_value_t = 1)]
    threads: usize,
}

const HOST_URL: &str = "https://mangapill.com";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let mut headers = HeaderMap::new();
    headers.insert(header::REFERER, HeaderValue::from_static(HOST_URL));

    let client = Client::builder()
        .default_headers(headers)
        .user_agent("Mozilla/5.0 (X11; Linux x86_64; rv:145.0) Gecko/20100101 Firefox/145.0")
        .build()
        .unwrap();

    let (title, title_url) = get_title_from_id(&client, args.id)
        .await
        .unwrap_or_else(|err| {
            eprintln!("{err}");
            std::process::exit(1);
        });

    let all_chapters = fetch_chapters_urls(&client, title_url.as_ref()).await;
    let mut selected_chapters = select_chapters(all_chapters);

    let book_path = Path::new("tmp").join(title);

    let num_chapters = selected_chapters.len();
    for Chapter { title, .. } in &selected_chapters {
        fs::create_dir_all(book_path.join(title)).await?;
    }
    fs::create_dir_all(PathBuf::from("output")).await?;

    let multi_progress = MultiProgress::new();
    let multi_progress_style = ProgressStyle::with_template(
        "{spinner:.yellow} [{bar:60.yellow/white}] {pos:>4}/{len} chaps - {msg}",
    )
    .unwrap()
    .tick_chars("⠒⠖⠔⠴⠤⠦⠢⠲ ")
    .progress_chars("-Cco");
    let total_progress = multi_progress.add(
        ProgressBar::new(num_chapters.try_into().unwrap()).with_style(
            ProgressStyle::with_template(
                "  [{bar:60.green/blue}] {pos:>4}/{len} chaps [{elapsed_precise}]{msg}",
            )
            .unwrap()
            .progress_chars("█▓▒░ "),
        ),
    );
    total_progress.enable_steady_tick(Duration::from_millis(250));

    let num_threads = args.threads.min(num_chapters);
    let batches = split_jobs(&mut selected_chapters, num_threads);

    let mut tasks: Vec<_> = vec![];
    for batch in batches {
        let client = client.clone();
        let book_path = book_path.clone();

        let chapter_progress = multi_progress.insert_before(
            &total_progress,
            ProgressBar::new(batch.len().try_into().unwrap())
                .with_style(multi_progress_style.clone()),
        );

        let total_progress = total_progress.clone();

        tasks.push(tokio::spawn(async move {
            download_chapters(
                &client,
                &batch,
                &book_path,
                &chapter_progress,
                &total_progress,
            )
            .await
        }));
    }

    for task in tasks {
        if let Err(e) = task.await {
            eprintln!("error: {e}");
        }
    }

    total_progress.set_style(ProgressStyle::with_template("{msg}").unwrap());
    total_progress.finish_with_message(format!(
        "Downloaded {num_chapters} {} to {path}/",
        if num_chapters > 1 {
            "chapters"
        } else {
            "chapter"
        },
        path = book_path.as_os_str().display()
    ));

    Ok(())
}
