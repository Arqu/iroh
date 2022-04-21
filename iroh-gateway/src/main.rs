use std::collections::HashMap;

use clap::Parser;
use iroh_gateway::{config::Config, handler::Handler};

#[derive(Parser, Debug, Clone)]
#[clap(author, version, about, long_about = None)]
struct Args {
    #[clap(short, long, required = false, default_value = "9050")]
    port: String,
    #[clap(short, long)]
    writeable: bool,
    #[clap(short, long)]
    fetch: bool,
    #[clap(short, long)]
    cache: bool,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    // hardcoded user headers
    let mut headers = HashMap::new();
    headers.insert("Access-Control-Allow-Origin".to_string(), "*".to_string());
    headers.insert("Access-Control-Allow-Headers".to_string(), "*".to_string());
    headers.insert("Access-Control-Allow-Methods".to_string(), "*".to_string());
    headers.insert(
        "Cache-Control".to_string(),
        "no-cache, no-transform".to_string(),
    );
    headers.insert("Accept-Ranges".to_string(), "none".to_string());

    let config = Config {
        port: args.port.clone(),
        writeable: args.writeable,
        fetch: args.fetch,
        cache: args.cache,
        headers,
    };
    println!("{:#?}", config);

    let handler = Handler::new(config);
    handler.serve().await;
}
