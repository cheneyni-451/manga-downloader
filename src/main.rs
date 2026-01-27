use std::{io::Write, path::Path, str::FromStr, time::Duration};

use chrono::Local;
use clap::Parser;
use futures::StreamExt;
use indicatif::{ProgressBar, ProgressStyle};
use lapin::{
    BasicProperties, Connection, ConnectionProperties,
    options::{
        BasicAckOptions, BasicCancelOptions, BasicConsumeOptions, BasicPublishOptions,
        QueueDeclareOptions, QueuePurgeOptions,
    },
    types::FieldTable,
    uri::AMQPUri,
};
use log::{debug, error, info};
use reqwest::{
    Client,
    header::{self, HeaderMap, HeaderValue},
};
use rkyv::rancor;
use tokio::{fs, task::JoinHandle};

use mangapill_scraper::{
    fetch::{fetch_chapters_urls, get_manga_display_name, get_title_from_id},
    models::Chapter,
    ui::select_chapter_range,
};

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

    let selected_chapters = match select_chapter_range(all_chapters) {
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
