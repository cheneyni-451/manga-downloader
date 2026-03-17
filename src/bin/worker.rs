use std::{io::Write, path::PathBuf, str::FromStr, time::Duration};

use chrono::Local;
use clap::Parser;
use log::{error, info};
use mangapill_scraper::fetch::download_chapter;
use redis::{
    Commands, RedisResult,
    streams::{
        StreamAutoClaimOptions, StreamAutoClaimReply, StreamClaimReply, StreamDeletionPolicy,
        StreamInfoConsumersReply, StreamReadOptions, StreamReadReply, XDelExStatusCode,
    },
};
use reqwest::{
    ClientBuilder,
    header::{self, HeaderMap, HeaderValue},
};

#[derive(Clone)]
struct Task {
    title: String,
    url: String,
}

#[derive(Parser, Debug, Clone)]
struct Args {
    #[arg(required = true, help = "stream key")]
    key: String,

    #[arg(required = true, help = "consumer group")]
    group: String,

    #[arg(required = true, help = "unique name of worker")]
    worker_id: String,

    #[arg(required = true, help = "path of the output directory")]
    manga_path: String,
}

const HOST_URL: &str = "https://mangapill.com";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let log_file = format!("log-{}.txt", args.worker_id.clone());
    let log_target = Box::new(
        std::fs::File::create(log_file.clone())
            .unwrap_or_else(|_| panic!("Failed to create {log_file}")),
    );
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
        .filter(Some("worker"), log::LevelFilter::Info)
        .filter(Some("mangapill_scraper"), log::LevelFilter::Info)
        .init();

    let Ok(mut redis_client) = redis::Client::open("redis://127.0.0.1/") else {
        error!("failed to connect to Redis server");
        std::process::exit(1);
    };
    let valid_consumer_id = match redis_client.xinfo_consumers(args.key.clone(), args.group.clone())
    {
        Ok(StreamInfoConsumersReply { consumers }) => {
            if consumers
                .iter()
                .find(|consumer| consumer.name == args.worker_id)
                .is_some()
            {
                error!("consumer with name '{}' already exists", args.worker_id);
                false
            } else {
                true
            }
        }
        Err(err) => {
            error!("{err}");
            false
        }
    };

    if !valid_consumer_id {
        std::process::exit(1);
    }
    let manga_path = PathBuf::from_str(&args.manga_path)?;
    let headers = HeaderMap::from_iter([(header::REFERER, HeaderValue::from_static(HOST_URL))]);
    let req_client = ClientBuilder::new()
        .default_headers(headers)
        .user_agent("Mozilla/5.0 (X11; Linux x86_64; rv:145.0) Gecko/20100101 Firefox/145.0")
        .build()?;

    const MIN_IDLE_TIME: u64 = 30_000;

    loop {
        let autoclaim_options = StreamAutoClaimOptions::default().count(1);
        let read_options = StreamReadOptions::default()
            .count(1)
            .block(10)
            .group(args.group.clone(), args.worker_id.clone());

        let id_task: Option<(String, Task)> = match redis_client.xautoclaim_options(
            args.key.clone(),
            args.group.clone(),
            args.worker_id.clone(),
            MIN_IDLE_TIME,
            0,
            autoclaim_options,
        ) {
            Ok(StreamAutoClaimReply { claimed, .. }) => {
                if claimed.is_empty() {
                    match redis_client.xread_options(
                        std::slice::from_ref(&args.key),
                        &[">"],
                        &read_options,
                    ) {
                        Ok(StreamReadReply { keys }) => {
                            if let Some(ids) = keys.first()
                                && let Some(claimed) = ids.ids.first()
                                && let Some(title) = claimed.get("title")
                                && let Some(url) = claimed.get("url")
                            {
                                Some((claimed.id.clone(), Task { title, url }))
                            } else {
                                None
                            }
                        }
                        Err(err) => {
                            error!("{err}");
                            break;
                        }
                    }
                } else {
                    if let Some(claimed) = claimed.first()
                        && let Some(title) = claimed.get("title")
                        && let Some(url) = claimed.get("url")
                    {
                        Some((claimed.id.clone(), Task { title, url }))
                    } else {
                        None
                    }
                }
            }
            Err(err) => {
                error!("{err}");
                break;
            }
        };

        // Didn't get a task
        if id_task.is_none() {
            let num_pending_msgs_result: RedisResult<usize> = redis_client.xgroup_delconsumer(
                args.key.clone(),
                args.group.clone(),
                args.worker_id.clone(),
            );
            match num_pending_msgs_result {
                Ok(num_pending_msgs) => {
                    info!(
                        "deleted consumer {} with {num_pending_msgs} pending messages",
                        args.worker_id.clone()
                    );
                }
                Err(err) => {
                    error!("{err}");
                    error!("failed to delete consumer {}", args.worker_id.clone());
                }
            };
            break;
        }

        // work on task
        let (
            task_id,
            Task {
                title: chapter_title,
                url,
            },
        ) = id_task.unwrap();

        let task_id_clone = task_id.clone();
        let args_clone = args.clone();
        let mut redis_client_clone = redis_client.clone();
        let ticker_handle = tokio::spawn(async move {
            let mut claim_ticker = tokio::time::interval(Duration::from_secs(5));

            loop {
                claim_ticker.tick().await;
                let res: RedisResult<StreamClaimReply> = redis_client_clone.xclaim(
                    args_clone.key.clone(),
                    args_clone.group.clone(),
                    args_clone.worker_id.clone(),
                    0,
                    std::slice::from_ref(&task_id_clone),
                );
            }
        });

        let chapter_url = format!("{HOST_URL}{url}");
        let chapter_path = manga_path.join(chapter_title);
        let _failed_pages = download_chapter(&req_client, &chapter_url, &chapter_path).await;

        if let Err(err) = redis_client.xack_del::<String, String, String, Vec<XDelExStatusCode>>(
            args.key.clone(),
            args.group.clone(),
            std::slice::from_ref(&task_id),
            StreamDeletionPolicy::DelRef,
        ) {
            error!("{err}");
        }
        ticker_handle.abort();
    }

    Ok(())
}
