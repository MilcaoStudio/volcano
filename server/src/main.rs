use std::fs;

use clap::Parser;
use volcano_sfu::rtc::config;

#[macro_use]
extern crate log;
#[macro_use]
extern crate serde;

pub mod signaling;

#[derive(clap::Parser)]
struct Cli {
    #[arg(short = 'c', long = "config", default_value = "./config.toml")]
    config_path: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    pretty_env_logger::init_timed();
    let file_content = {
        // Drop cli before server launch
        let cli = Cli::parse();
        info!("Reading config file:{}", cli.config_path);
        fs::read_to_string(cli.config_path)
    };
    let config = match &file_content{
        Ok(data) => {
            config::load(data).inspect_err(|e| error!("Error loading config data: {e}.\nLoading default config.")).unwrap_or_default()
        },
        Err(e) => {
            error!("Error loading config file: {e}.\nLoading default config.");
            config::Config::default()
        }
    };
  
    signaling::server::launch("0.0.0.0:4000", config, Box::new(move |token| {
        Box::pin(async move {
            use signaling::server::{UserCapabilities, UserInformation};

            let id = token.to_string();

            Ok(UserInformation {
                id,
                capabilities: UserCapabilities {
                    audio: true,
                    video: true,
                    screenshare: true,
                },
            })
        })
    })).await
}