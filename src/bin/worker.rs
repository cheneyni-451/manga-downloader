use std::{path::PathBuf, str::FromStr};

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
use mangapill_scraper::{fetch::download_chapter, models::Chapter};
use reqwest::{
    ClientBuilder,
    header::{self, HeaderMap, HeaderValue},
};
use rkyv::rancor;

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
