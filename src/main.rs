use std::{
    error::Error,
    fs::{self, File},
    io::Write,
    path::PathBuf,
    thread::{self},
};

use clap::Parser;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use reqwest::{
    blocking::Client,
    header::{self, HeaderMap, HeaderValue},
};
use scraper::Selector;

fn download_file(client: &Client, url: &str, file_path: PathBuf) -> Result<(), Box<dyn Error>> {
    let response = client.get(url).send()?;
    let content = response.bytes()?;

    let mut downloaded_file = File::create(file_path)?;
    downloaded_file.write_all(&content)?;

    Ok(())
}

fn download_chapter(
    client: &Client,
    chapter_url: &str,
    chapter_title: &str,
    args: &Args,
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

                let page_path = PathBuf::from(format!(
                    "tmp/{title}/{chapter_title}/{page_num:03}.jpg",
                    title = args.title
                ));

                download_file(client, page_url, page_path).unwrap();
                progress_bar.tick();
            });
    }
}

#[derive(Debug, Clone)]
struct Chapter {
    url: String,
    title: String,
}

fn get_num_chapter_urls(client: &Client, title_url: &str, args: &Args) -> Vec<Chapter> {
    let response = client.get(title_url).send();
    if let Ok(result) = response {
        let html_content = result.text().unwrap_or_default();
        let doc = scraper::Html::parse_document(&html_content);

        let selector = Selector::parse("#chapters a").unwrap();
        doc.select(&selector)
            .take(args.last_n)
            .map(|a| Chapter {
                url: a.attr("href").unwrap_or_default().to_string(),
                title: a
                    .attr("title")
                    .unwrap_or_default()
                    .trim()
                    .to_ascii_lowercase()
                    .to_string(),
            })
            .collect()
    } else {
        vec![]
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
    batches.extend(jobs.chunks_exact(chunk_size).map(|c| c.to_owned()));

    batches
}

fn download_chapters(
    client: &Client,
    args: &Args,
    chapters: &Vec<Chapter>,
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
        download_chapter(client, &chapter_url, chapter_title, args, chapter_progress);

        chapter_progress.inc(1);
        total_progress.inc(1);
    }

    chapter_progress.finish();
}

#[derive(Parser, Debug, Clone)]
struct Args {
    #[arg(
        required = true,
        help = "Title of manga in the URL: mangapill.com/manga/<ID>/<TITLE>"
    )]
    title: String,

    #[arg(
        required = true,
        help = "ID of manga in the URL: mangapill.com/manga/<ID>/<TITLE>"
    )]
    id: usize,

    #[arg(short = 'j', long, default_value_t = 1)]
    threads: usize,

    #[arg(short = 'n', long="last-n", default_value_t = usize::MAX)]
    last_n: usize,
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

    let mut chapters = get_num_chapter_urls(
        &client,
        &format!(
            "{HOST_URL}/manga/{id}/{title}",
            id = args.id,
            title = args.title
        ),
        &args,
    );

    let book_path = format!("tmp/{}", args.title);

    let num_chapters = chapters.len();
    chapters.iter().for_each(|Chapter { url: _, title }| {
        fs::create_dir_all(format!("{book_path}/{title}")).unwrap();
    });

    let num_threads = args.threads.min(num_chapters);
    let batches = split_jobs(&mut chapters, num_threads);

    let multi_progress = MultiProgress::new();
    let multi_progress_style = ProgressStyle::with_template(
        "{spinner:.yellow} [{bar:60.yellow/white}] {pos:>4}/{len} chaps - {msg}",
    )
    .unwrap()
    .tick_chars("\\|/- ")
    .progress_chars("-Cco");
    let total_progress = multi_progress.add(ProgressBar::new(num_chapters.try_into().unwrap()));
    total_progress.set_style(
        ProgressStyle::with_template(
            "  [{bar:60.green/blue}] {pos:>4}/{len} chaps [{elapsed_precise}]",
        )
        .unwrap()
        .progress_chars("█▓▒░ "),
    );
    total_progress.tick();

    let mut threads = vec![];

    for batch in batches {
        let args = args.clone();
        let client = client.clone();

        let chapter_progress = ProgressBar::new(batch.len().try_into().unwrap());
        chapter_progress.set_style(multi_progress_style.clone());
        let chapter_progress = multi_progress.insert_before(&total_progress, chapter_progress);

        let total_progress = total_progress.clone();

        threads.push(thread::spawn(move || {
            download_chapters(&client, &args, &batch, &chapter_progress, &total_progress)
        }));
    }

    threads.into_iter().for_each(|h| h.join().unwrap());
    total_progress.finish_with_message(format!("downloaded {num_chapters} chapters to "));
}
