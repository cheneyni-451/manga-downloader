use std::{io::Write, path::Path, time::Duration};

use chrono::Local;
use clap::Parser;
use indicatif::{ProgressBar, ProgressStyle};
use log::{debug, error, info};
use redis::{Commands, RedisResult};
use reqwest::{
    Client,
    header::{self, HeaderMap, HeaderValue},
};
use tokio::{fs, time::sleep};

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
const STREAM_KEY: &str = "mangapill_scraper_queue";
const CONSUMER_GROUP: &str = "mangapill_scraper_workers";

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

    let Ok(mut redis_client) = redis::Client::open("redis://127.0.0.1/") else {
        error!("failed to connect to Redis");
        std::process::exit(1);
    };
    info!("connected to queue service");
    let _: RedisResult<()> = redis_client.xgroup_create_mkstream(STREAM_KEY, CONSUMER_GROUP, 0);

    let total_progress = ProgressBar::new(num_chapters.try_into().unwrap()).with_style(
        ProgressStyle::with_template(
            "  [{bar:60.green/blue}] {pos:>4}/{len} chaps [{elapsed_precise}]{msg}",
        )
        .unwrap()
        .progress_chars("█▓▒░ "),
    );
    total_progress.enable_steady_tick(Duration::from_millis(250));

    let num_workers = args.threads.min(num_chapters);
    let workers: Vec<_> = (0..num_workers)
        .filter_map(|i| {
            std::process::Command::new("./target/release/worker")
                .arg(STREAM_KEY)
                .arg(CONSUMER_GROUP)
                .arg(format!("worker-{i}"))
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
        let res: RedisResult<String> = redis_client.xadd(
            STREAM_KEY,
            "*",
            &[("title", chapter.title), ("url", chapter.url)],
        );
    }

    let mut redis_client_clone = redis_client.clone();
    let total_progress_clone = total_progress.clone();
    let ticker_handle = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_millis(250));

        loop {
            ticker.tick().await;
            let num_tasks_remaining: RedisResult<usize> = redis_client_clone.xlen(STREAM_KEY);
            total_progress_clone.set_position(
                (num_chapters.saturating_sub(num_tasks_remaining.unwrap_or(num_chapters))) as u64,
            );
        }
    });

    loop {
        if let Ok(remaining_requests) = redis_client.xlen::<&str, u64>(STREAM_KEY)
            && remaining_requests == 0
        {
            break;
        } else {
            sleep(Duration::from_secs(1)).await;
        }
    }
    ticker_handle.abort();
    if let Ok(1) = redis_client.xgroup_destroy(STREAM_KEY, CONSUMER_GROUP) {
        debug!("destroyed consumer group");
    } else {
        debug!("failed to destroy consumer group");
    }

    let mut all_failed_chapters = vec![];

    for mut worker in workers {
        let exit_status = worker.wait();
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
