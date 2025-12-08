use std::{
    error::Error,
    fs::{self, File},
    io::Write,
    path::PathBuf,
    thread::{self, JoinHandle},
};

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

fn download_chapter(client: Client, chapter_url: &str, chapter_num: usize) {
    let response = client.get(chapter_url).send();
    if let Ok(result) = response {
        let html_content = result.text().unwrap_or_default();
        let doc = scraper::Html::parse_document(&html_content);

        let selector = Selector::parse("div>chapter-page img").unwrap();
        let num_pages = doc.select(&selector).count();

        for page_num in 1..=num_pages {
            let page_url = format!(
                "https://cdn.readdetectiveconan.com/file/mangap/3520/10{chapter_num:03}000/{page_num}.jpeg"
            );
            let page_path = PathBuf::from(format!(
                "tmp/ranma/chapter_{chapter_num:03}/{page_num:02}.jpg"
            ));

            download_file(client.clone(), &page_url, page_path).unwrap();
        }
    }
}

fn main() {
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

    for chapter_num in 1..=407 {
        fs::create_dir_all(format!("tmp/ranma/chapter_{chapter_num:03}")).unwrap();
    }

    let mut handles = Vec::with_capacity(11);
    for worker in 0..11 {
        let client = client.clone();
        handles.push(thread::spawn(move || {
                let start = worker * 37 + 1;
                let end = (worker + 1) * 37;
                for chapter_num in start..=end {
                    let chapter_url = format!(
                        "https://mangapill.com/chapters/3520-10{chapter_num:03}000/ranma-chapter-{chapter_num}"
                    );
                    download_chapter(client.clone(), &chapter_url, chapter_num);
                }
            }));
    }

    handles
        .into_iter()
        .for_each(|handle| handle.join().unwrap());
}
