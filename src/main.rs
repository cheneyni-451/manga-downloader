use std::{error::Error, fmt::Display, fs, io::Write, path::Path, thread};

use clap::Parser;
use dialoguer::{Select, console::Style, theme::ColorfulTheme};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use reqwest::{
    Url,
    blocking::Client,
    header::{self, HeaderMap, HeaderValue},
};
use scraper::Selector;

fn download_file(client: &Client, url: &str, file_path: &Path) -> Result<(), Box<dyn Error>> {
    let response = client.get(url).send()?;
    let content = response.bytes()?;

    let mut downloaded_file = fs::File::create(file_path)?;
    downloaded_file.write_all(&content)?;

    Ok(())
}

fn download_chapter(
    client: &Client,
    chapter_url: &str,
    chapter_path: &Path,
    progress_bar: &ProgressBar,
) {
    let response = client.get(chapter_url).send();
    if let Ok(result) = response {
        let html_content = result.text().unwrap_or_default();
        let doc = scraper::Html::parse_document(&html_content);

        let selector = Selector::parse("div>chapter-page img").unwrap();
        let images = doc.select(&selector);
        images
            .into_iter()
            .enumerate()
            .for_each(|(page_num, image)| {
                let page_url = image
                    .attr("src")
                    .unwrap_or_else(|| image.attr("data-src").unwrap_or_default());

                let page_path = chapter_path.join(format!("{page_num:03}.jpg"));

                download_file(client, page_url, &page_path).unwrap();
                progress_bar.tick();
            });
    }
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

fn fetch_chapters_urls(client: &Client, title_url: &str) -> Vec<Chapter> {
    match client.get(title_url).send() {
        Ok(response) => {
            let html_content = response.text().unwrap_or_default();
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

fn get_title_from_id(client: &Client, id: usize) -> Option<(String, Url)> {
    if let Ok(response) = client.get(format!("{HOST_URL}/manga/{id}")).send() {
        let url = response.url();
        match url.path_segments() {
            Some(mut segments) => {
                let title = segments.next_back().unwrap();
                return Some((title.to_string(), url.clone()));
            }
            None => return None,
        };
    }

    None
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

fn download_chapters(
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
        download_chapter(
            client,
            &chapter_url,
            &manga_path.join(chapter_title),
            chapter_progress,
        );

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

fn main() {
    let args = Args::parse();

    let mut headers = HeaderMap::new();
    headers.insert(header::REFERER, HeaderValue::from_static(HOST_URL));

    let client = Client::builder()
        .default_headers(headers)
        .user_agent("Mozilla/5.0 (X11; Linux x86_64; rv:145.0) Gecko/20100101 Firefox/145.0")
        .build()
        .unwrap();

    let (title, title_url) = get_title_from_id(&client, args.id).unwrap_or_else(|| {
        eprintln!("Invalid manga id: {}", args.id);
        std::process::exit(1);
    });

    let all_chapters = fetch_chapters_urls(&client, title_url.as_ref());
    let mut selected_chapters = select_chapters(all_chapters);

    let book_path = Path::new("tmp").join(title);

    let num_chapters = selected_chapters.len();
    for Chapter { title, .. } in &selected_chapters {
        fs::create_dir_all(book_path.join(title)).unwrap();
    }

    let multi_progress = MultiProgress::new();
    let multi_progress_style = ProgressStyle::with_template(
        "{spinner:.yellow} [{bar:60.yellow/white}] {pos:>4}/{len} chaps - {msg}",
    )
    .unwrap()
    .tick_chars("⠒⠖⠔⠴⠤⠦⠢⠲ ")
    .progress_chars("-Cco");
    let total_progress = multi_progress.add(ProgressBar::new(num_chapters.try_into().unwrap()));
    total_progress.set_style(
        ProgressStyle::with_template(
            "  [{bar:60.green/blue}] {pos:>4}/{len} chaps [{elapsed_precise}]{msg}",
        )
        .unwrap()
        .progress_chars("█▓▒░ "),
    );
    total_progress.tick();

    let mut threads = vec![];

    let num_threads = args.threads.min(num_chapters);
    let batches = split_jobs(&mut selected_chapters, num_threads);

    for batch in batches {
        let client = client.clone();
        let book_path = book_path.clone();

        let chapter_progress = multi_progress.insert_before(
            &total_progress,
            ProgressBar::new(batch.len().try_into().unwrap())
                .with_style(multi_progress_style.clone()),
        );

        let total_progress = total_progress.clone();

        threads.push(thread::spawn(move || {
            download_chapters(
                &client,
                &batch,
                &book_path,
                &chapter_progress,
                &total_progress,
            )
        }));
    }

    threads.into_iter().for_each(|h| h.join().unwrap());
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
}
