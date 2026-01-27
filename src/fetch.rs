use std::path::Path;

use futures::StreamExt;
use log::{debug, error};
use reqwest::{Client, Url};
use scraper::Selector;
use tokio::{fs, io::AsyncWriteExt};

use crate::{errors::ScraperErrors, models::Chapter};

const HOST_URL: &str = "https://mangapill.com";

pub async fn get_title_from_id(client: &Client, id: usize) -> anyhow::Result<(String, Url)> {
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

pub async fn get_manga_display_name(client: &Client, url: &str) -> anyhow::Result<Option<String>> {
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

pub async fn fetch_chapters_urls(client: &Client, title_url: &str) -> anyhow::Result<Vec<Chapter>> {
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

pub async fn download_chapter(
    client: &Client,
    chapter_url: &str,
    chapter_path: &Path,
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
            download_file(client, &page_url, chapter_path, page_num).await
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

async fn download_file(
    client: &Client,
    url: &str,
    chapter_path: &Path,
    page_num: usize,
) -> anyhow::Result<()> {
    let fetch_image = async move || client.get(url).send().await?.bytes().await;
    match fetch_image().await {
        Ok(data) => {
            let file_path = chapter_path.join(format!("{page_num:03}.jpg"));
            let mut downloaded_file = fs::File::create(file_path).await?;
            downloaded_file.write_all(&data).await?;

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
