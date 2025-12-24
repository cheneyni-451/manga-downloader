use std::{fmt::Display, io::Write, path::Path, time::Duration};

use chrono::Local;
use clap::Parser;
use dialoguer::{Select, console::Style, theme::ColorfulTheme};
use futures::stream::StreamExt;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use log::{debug, error, info};
use reqwest::{
    Client, Url,
    header::{self, HeaderMap, HeaderValue},
};
use scraper::Selector;
use tokio::{fs, io::AsyncWriteExt, time::sleep};

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
            .enumerate()
            .filter_map(|(i, img)| {
                img.attr("src")
                    .or_else(|| img.attr("data-src"))
                    .or_else(|| {
                        debug!("img element with missing url: {img:?}");
                        error!("failed to extract url for page {}", i + 1);
                        None
                    })
                    .map(str::to_string)
            })
            .collect::<Vec<String>>())
    }

    let image_urls = fetch_image_urls(client, chapter_url).await?;

    let tasks = futures::stream::iter(image_urls)
        .enumerate()
        .map(|(page_num, page_url)| async move {
            download_file(client, &page_url, chapter_path, page_num, progress_bar).await
        })
        .buffer_unordered(6);

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

async fn fetch_chapters_urls(client: &Client, title_url: &str) -> anyhow::Result<Vec<Chapter>> {
    let html_content = client.get(title_url).send().await?.text().await?;
    let doc = scraper::Html::parse_document(&html_content);

    let selector = Selector::parse("#chapters a").unwrap();
    let chapters = doc.select(&selector);
    Ok(chapters
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
        .collect())
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
            if title.parse::<usize>().is_ok() {
                Err(ScraperErrors::InvalidBookId(id).into())
            } else {
                Ok((title.to_string(), url.clone()))
            }
        }
        None => Err(ScraperErrors::InvalidBookId(id).into()),
    }
}

async fn get_manga_display_name(client: &Client, url: &str) -> anyhow::Result<Option<String>> {
    let html_content = client.get(url).send().await?.text().await?;
    let doc = scraper::Html::parse_document(&html_content);

    let selector = Selector::parse("h1").unwrap();
    let mut h1 = doc.select(&selector);
    Ok(h1
        .next()
        .or_else(|| {
            error!("failed to get title");
            None
        })
        .map(|e| e.text().collect::<String>()))
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
) -> Vec<Chapter> {
    chapter_progress.tick();
    let mut failed_chapter_downloads = vec![];

    for chapter @ Chapter {
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
            Ok(failed_pages) => {
                if !failed_pages.is_empty() {
                    error!(
                        "{chapter_title}: failed to download {} pages",
                        failed_pages.len()
                    );
                    failed_chapter_downloads.push(chapter.clone());
                }
            }
            Err(err) => {
                debug!("{err}");
                error!("failed to fetch: {url}");
                failed_chapter_downloads.push(chapter.clone());
            }
        };

        chapter_progress.inc(1);
        total_progress.inc(1);
    }

    chapter_progress.finish();

    failed_chapter_downloads
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
    let log_target = Box::new(std::fs::File::create("log.txt").expect("Failed to create log.txt"));
    env_logger::Builder::new()
        .format(|buf, record| {
            writeln!(
                buf,
                "{} [{}] {}",
                Local::now().format("%Y-%m-%dT%H:%M:%S%.6f"),
                record.level(),
                record.args(),
            )
        })
        .target(env_logger::Target::Pipe(log_target))
        .filter(Some("mangapill_scraper"), log::LevelFilter::Debug)
        .init();

    let args = Args::parse();
    debug!("parsed args: {args:?}");

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
            error!("{err}");
            std::process::exit(1);
        });
    let display_title = get_manga_display_name(&client, title_url.as_ref())
        .await
        .unwrap_or_else(|_| Some(title.clone()))
        .unwrap();
    println!("Select chapters to download for {display_title}");

    let all_chapters = match fetch_chapters_urls(&client, title_url.as_ref()).await {
        Ok(chapters) => {
            if chapters.is_empty() {
                error!("no chapter urls fetched");
                eprintln!("no chapter urls fetched");
                std::process::exit(1);
            }
            chapters
        }
        Err(err) => {
            debug!("{err}");
            error!("failed to fetch manga page: {title_url}");
            eprintln!("no chapter urls fetched");
            std::process::exit(1);
        }
    };

    let mut selected_chapters = select_chapters(all_chapters);

    let book_path = Path::new("tmp").join(title);

    let num_chapters = selected_chapters.len();
    for Chapter { title, .. } in &selected_chapters {
        fs::create_dir_all(book_path.join(title))
            .await
            .inspect_err(|e| error!("{e}"))?;
    }

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
            let failed_chapters = download_chapters(
                &client,
                &batch,
                &book_path,
                &chapter_progress,
                &total_progress,
            )
            .await;
            chapter_progress.finish_and_clear();

            failed_chapters
        }));
    }
    let mut all_failed_chapters = vec![];
    let start_time = Local::now();
    for task in tasks {
        match task.await {
            Ok(failed_chapters) => {
                all_failed_chapters.extend(failed_chapters);
            }
            Err(e) => {
                error!("{e}");
            }
        }
    }
    let end_time = Local::now();
    let download_duration = end_time.signed_duration_since(start_time);
    info!(
        "finished downloading in {:.6} seconds",
        download_duration.as_seconds_f64()
    );
    if !all_failed_chapters.is_empty() {
        info!(
            "failed to fully download chapters: [{}]",
            all_failed_chapters
                .iter()
                .map(|Chapter { title, .. }| title.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    total_progress.finish();
    println!(
        "Downloaded {num_chapters} {} in {duration:.2} seconds to {path}/",
        if num_chapters > 1 {
            "chapters"
        } else {
            "chapter"
        },
        duration = download_duration.as_seconds_f64(),
        path = book_path.as_os_str().display()
    );

    Ok(())
}
