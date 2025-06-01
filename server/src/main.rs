#[macro_use]
extern crate log;
#[macro_use]
extern crate serde;

pub mod signaling;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    pretty_env_logger::init_timed();
    signaling::server::launch("0.0.0.0:4000", Box::new(move |token| {
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
