use std::{io::Write, path::Path, str::FromStr, time::Duration};

use chrono::Local;
use clap::Parser;
use dialoguer::{Select, console::Style, theme::ColorfulTheme};
use futures::StreamExt;
use indicatif::{ProgressBar, ProgressStyle};
use lapin::{
    BasicProperties, Connection, ConnectionProperties,
    options::{
        BasicAckOptions, BasicCancelOptions, BasicConsumeOptions, BasicPublishOptions,
        QueueDeclareOptions, QueuePurgeOptions,
    },
    protocol::confirm,
    types::FieldTable,
    uri::AMQPUri,
};
use log::{debug, error, info};
use reqwest::{
    Client, Url,
    header::{self, HeaderMap, HeaderValue},
};
use rkyv::rancor;
use scraper::Selector;
use tokio::{fs, task::JoinHandle};

use mangapill_scraper::{errors::ScraperErrors, models::Chapter};

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

fn select_chapters(mut chapters: Vec<Chapter>) -> anyhow::Result<Vec<Chapter>> {
    let selection_theme = ColorfulTheme {
        prompt_style: Style::default().blue(),
        active_item_style: Style::default().reverse(),
        ..Default::default()
    };
    let chapter_selection_start = Select::with_theme(&selection_theme)
        .with_prompt("First of the chapters to download")
        .items(&chapters)
        .max_length(10)
        .interact_opt()?;
    match chapter_selection_start {
        Some(start_selection) => {
            debug!("selected chapter range start index: {start_selection}");
            chapters.drain(..start_selection);
        }
        None => return Err(ScraperErrors::InvalidChapterSelection.into()),
    };

    let chapter_selection_end = Select::with_theme(&selection_theme)
        .with_prompt("Last of the chapters to download")
        .items(&chapters)
        .max_length(10)
        .interact_opt()?;
    match chapter_selection_end {
        Some(end_selection) => {
            debug!("selected chapter range end index: {end_selection}");
            chapters.truncate(end_selection.saturating_add(1));
            Ok(chapters)
        }
        None => Err(ScraperErrors::InvalidChapterSelection.into()),
    }
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

    let selected_chapters = match select_chapters(all_chapters) {
        Ok(selected_chapters) => {
            info!(
                "selected chapters [{} - {}]",
                selected_chapters.first().unwrap().title,
                selected_chapters.last().unwrap().title
            );
            selected_chapters
        }
        Err(err) => {
            debug!("{err}");
            error!("failed to select chapters");
            std::process::exit(1);
        }
    };

    let book_path = Path::new("tmp").join(title);

    let num_chapters = selected_chapters.len();
    for Chapter { title, .. } in &selected_chapters {
        fs::create_dir_all(book_path.join(title))
            .await
            .inspect_err(|e| error!("{e}"))?;
    }

    let amqp_addr = "amqp://127.0.0.1:5672/%2f";
    let conn = Connection::connect_uri(
        AMQPUri::from_str(amqp_addr).unwrap_or_else(|err| {
            error!("{err}");
            std::process::exit(1);
        }),
        ConnectionProperties::default().with_connection_name("chapter_queue".into()),
    )
    .await?;
    info!("connected to queue service");
    let send_channel = conn.create_channel().await?;
    send_channel
        .queue_declare(
            "chapter_queue",
            QueueDeclareOptions {
                durable: true,
                ..Default::default()
            },
            FieldTable::default(),
        )
        .await?;
    send_channel
        .queue_purge("chapter_queue", QueuePurgeOptions::default())
        .await?;

    let reply_channel = conn.create_channel().await?;
    reply_channel
        .queue_declare(
            "chapter_completed_queue",
            QueueDeclareOptions {
                durable: true,
                ..Default::default()
            },
            FieldTable::default(),
        )
        .await?;
    reply_channel
        .queue_purge("chapter_completed_queue", QueuePurgeOptions::default())
        .await?;

    let total_progress = ProgressBar::new(num_chapters.try_into().unwrap()).with_style(
        ProgressStyle::with_template(
            "  [{bar:60.green/blue}] {pos:>4}/{len} chaps [{elapsed_precise}]{msg}",
        )
        .unwrap()
        .progress_chars("█▓▒░ "),
    );
    total_progress.enable_steady_tick(Duration::from_millis(250));

    let mut reply_consumer = reply_channel
        .basic_consume(
            "chapter_completed_queue",
            "main",
            BasicConsumeOptions::default(),
            FieldTable::default(),
        )
        .await?;
    let total_progress_clone = total_progress.clone();
    let reply_handle: JoinHandle<anyhow::Result<()>> = tokio::spawn(async move {
        while let Some(delivery) = reply_consumer.next().await {
            match delivery {
                Ok(delivery) => {
                    if let Ok(Chapter { .. }) =
                        rkyv::from_bytes::<Chapter, rancor::Error>(&delivery.data)
                    {
                        total_progress_clone.inc(1);

                        delivery.ack(BasicAckOptions::default()).await?;

                        if total_progress_clone.position() == total_progress_clone.length().unwrap()
                        {
                            break;
                        }
                    } else {
                        delivery.ack(BasicAckOptions::default()).await?;
                    }
                }
                Err(err) => {
                    error!("{err}");
                }
            }
        }
        reply_channel
            .basic_cancel("main", BasicCancelOptions::default())
            .await?;

        Ok(())
    });

    let num_workers = args.threads.min(num_chapters);
    let workers: Vec<_> = (0..num_workers)
        .filter_map(|_| {
            std::process::Command::new("./target/release/worker")
                .arg(book_path.to_str().unwrap())
                .spawn()
                .ok()
        })
        .collect();
    if workers.is_empty() {
        error!("failed to spawn workers");
        std::process::exit(1);
    }

    let start_time = Local::now();
    for chapter in selected_chapters {
        let confirm = send_channel
            .basic_publish(
                "",
                "chapter_queue",
                BasicPublishOptions::default(),
                &rkyv::to_bytes::<rancor::Error>(&chapter).unwrap(),
                BasicProperties::default().with_delivery_mode(2),
            )
            .await?
            .await?;
    }
    for _ in 0..workers.len() {
        let confirm = send_channel
            .basic_publish(
                "",
                "chapter_queue",
                BasicPublishOptions::default(),
                "end".as_bytes(),
                BasicProperties::default().with_delivery_mode(2),
            )
            .await?
            .await?;
    }
    let mut all_failed_chapters = vec![];

    for mut worker in workers {
        let exit_status = worker.wait()?;
    }
    reply_handle.await?;
    send_channel
        .basic_cancel("main", BasicCancelOptions::default())
        .await?;

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
