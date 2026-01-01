use std::{
    path::{Path, PathBuf},
    str::FromStr,
};

use clap::Parser;
use futures::StreamExt;
use lapin::{
    BasicProperties, Connection, ConnectionProperties,
    options::{
        BasicAckOptions, BasicCancelOptions, BasicConsumeOptions, BasicPublishOptions,
        BasicQosOptions, QueueDeclareOptions,
    },
    types::FieldTable,
    uri::AMQPUri,
};
use log::{debug, error};
use mangapill_scraper::{errors::ScraperErrors, models::Chapter};
use reqwest::{
    Client, ClientBuilder,
    header::{self, HeaderMap, HeaderValue},
};
use rkyv::rancor;
use scraper::Selector;
use tokio::{fs, io::AsyncWriteExt};

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

async fn download_chapter(
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

#[derive(Parser, Debug, Clone)]
struct Args {
    #[arg(required = true, help = "path of the output directory")]
    manga_path: String,
}

const HOST_URL: &str = "https://mangapill.com";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let amqp_addr = "amqp://127.0.0.1:5672/%2f";
    let conn = Connection::connect_uri(
        AMQPUri::from_str(amqp_addr).unwrap_or_else(|err| {
            eprintln!("{err}");
            std::process::exit(1);
        }),
        ConnectionProperties::default().with_connection_name("chapter_queue_worker".into()),
    )
    .await?;

    let recv_channel = conn.create_channel().await?;
    let send_channel = conn.create_channel().await?;
    send_channel
        .queue_declare(
            "chapter_completed_queue",
            QueueDeclareOptions {
                durable: true,
                ..Default::default()
            },
            FieldTable::default(),
        )
        .await?;

    const QUEUE_NAME: &str = "chapter_queue";

    recv_channel
        .queue_declare(
            QUEUE_NAME,
            QueueDeclareOptions {
                durable: true,
                ..Default::default()
            },
            FieldTable::default(),
        )
        .await?;

    recv_channel
        .basic_qos(1, BasicQosOptions::default())
        .await?;
    let mut consumer = recv_channel
        .basic_consume(
            QUEUE_NAME,
            "worker",
            BasicConsumeOptions::default(),
            FieldTable::default(),
        )
        .await?;

    let manga_path = PathBuf::from_str(&args.manga_path)?;
    let mut headers = HeaderMap::new();
    headers.insert(header::REFERER, HeaderValue::from_static(HOST_URL));
    let client = ClientBuilder::new()
        .default_headers(headers)
        .user_agent("Mozilla/5.0 (X11; Linux x86_64; rv:145.0) Gecko/20100101 Firefox/145.0")
        .build()?;

    while let Some(delivery) = consumer.next().await {
        match delivery {
            Ok(delivery) => {
                if let Ok(
                    chapter @ Chapter {
                        url,
                        title: chapter_title,
                    },
                ) = &rkyv::from_bytes::<Chapter, rancor::Error>(&delivery.data)
                {
                    let chapter_url = format!("{HOST_URL}{url}");
                    let _failed_pages =
                        download_chapter(&client, &chapter_url, &manga_path.join(chapter_title))
                            .await?;

                    delivery.ack(BasicAckOptions::default()).await?;
                    send_channel
                        .basic_publish(
                            "",
                            "chapter_completed_queue",
                            BasicPublishOptions::default(),
                            &rkyv::to_bytes::<rancor::Error>(chapter).unwrap(),
                            BasicProperties::default().with_delivery_mode(2),
                        )
                        .await?
                        .await?;
                } else {
                    delivery.ack(BasicAckOptions::default()).await?;
                    break;
                }
            }
            Err(err) => {
                eprintln!("{err}");
                break;
            }
        }
    }

    recv_channel
        .basic_cancel("worker", BasicCancelOptions::default())
        .await?;

    Ok(())
}
