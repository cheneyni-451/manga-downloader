use std::{
    error::Error,
    fs::{self, File},
    io::Write,
    path::PathBuf,
    thread::{self},
};

use clap::Parser;
use reqwest::{
    blocking::Client,
    header::{self, HeaderMap, HeaderValue},
};
use scraper::Selector;

fn download_file(client: Client, url: &str, file_path: PathBuf) -> Result<(), Box<dyn Error>> {
    let response = client.get(url).send()?;
    let content = response.bytes()?;

    let mut downloaded_file = File::create(file_path)?;
    downloaded_file.write_all(&content)?;

    Ok(())
}

fn download_chapter(client: Client, chapter_url: &str, chapter_title: &str, args: Args) {
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
                    .unwrap_or(image.attr("data-src").unwrap_or_default());

                let page_path = PathBuf::from(format!(
                    "tmp/{title}/{chapter_title}/{page_num:03}.jpg",
                    title = args.title
                ));

                download_file(client.clone(), page_url, page_path).unwrap();
            });
    }
}

#[derive(Debug, Clone)]
struct Chapter {
    url: String,
    title: String,
}

fn get_num_chapter_urls(client: Client, title_url: &str) -> Vec<Chapter> {
    let response = client.get(title_url).send();
    if let Ok(result) = response {
        let html_content = result.text().unwrap_or_default();
        let doc = scraper::Html::parse_document(&html_content);

        let selector = Selector::parse("#chapters a").unwrap();
        doc.select(&selector)
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

#[derive(Parser, Debug, Clone)]
struct Args {
    #[arg(short, long)]
    title: String,

    #[arg(long)]
    id: usize,

    #[arg(short = 'j', long, default_value_t = 1)]
    threads: usize,
}

fn main() {
    let args = Args::parse();

    let mut headers = HeaderMap::new();
    headers.insert(
        header::REFERER,
        HeaderValue::from_static("https://mangapill.com/"),
    );

    let client = Client::builder()
        .default_headers(headers)
        .user_agent("Mozilla/5.0 (X11; Linux x86_64; rv:145.0) Gecko/20100101 Firefox/145.0")
        .build()
        .unwrap();

    let title_url = format!(
        "https://mangapill.com/manga/{id}/{title}",
        id = args.id,
        title = args.title
    );
    let chapters = get_num_chapter_urls(client.clone(), &title_url);

    chapters.iter().for_each(|Chapter { url: _, title }| {
        fs::create_dir_all(format!("tmp/{book_title}/{title}", book_title = args.title)).unwrap();
    });

    let mut handles = vec![];
    chapters
        .chunks(chapters.len() / args.clone().threads)
        .map(|c| c.to_owned())
        .for_each(|chunk| {
            let args = args.clone();
            let client = client.clone();
            handles.push(thread::spawn(move || {
                let chunk_size = chunk.len();
                chunk
                    .iter()
                    .enumerate()
                    .for_each(|(i, Chapter { url, title })| {
                        let chapter_url = format!("https://mangapill.com{url}");
                        // println!("{chapter_url}");
                        download_chapter(client.clone(), &chapter_url, title, args.clone());
                        println!("downloaded {title:<13} {}/{}", i + 1, chunk_size);
                    });
            }))
        });

    handles.into_iter().for_each(|h| h.join().unwrap());
}
