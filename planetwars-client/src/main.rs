pub mod pb {
    tonic::include_proto!("grpc.planetwars.client_api");

    pub use player_api_client_message::ClientMessage as PlayerApiClientMessageType;
    pub use player_api_server_message::ServerMessage as PlayerApiServerMessageType;
}

use clap::Parser;
use pb::client_api_service_client::ClientApiServiceClient;
use planetwars_matchrunner::bot_runner::Bot;
use serde::Deserialize;
use std::{path::PathBuf, time::Duration};
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tonic::{metadata::MetadataValue, transport::Channel, Request, Status};

#[derive(clap::Parser)]
struct PlayMatch {
    #[clap(value_parser)]
    bot_config_path: String,

    #[clap(value_parser)]
    opponent_name: String,

    #[clap(value_parser, long = "map")]
    map_name: Option<String>,

    #[clap(
        value_parser,
        long,
        default_value = "https://planetwars.dev:7492",
        env = "PLANETWARS_GRPC_SERVER_URL"
    )]
    grpc_server_url: String,
}

#[derive(Deserialize)]
struct BotConfig {
    #[allow(dead_code)]
    name: Option<String>,
    command: Command,
    working_directory: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Command {
    String(String),
    Argv(Vec<String>),
}

impl Command {
    pub fn to_argv(&self) -> Vec<String> {
        match self {
            Command::Argv(vec) => vec.clone(),
            Command::String(s) => shlex::split(s).expect("invalid command string"),
        }
    }
}

#[tokio::main]
async fn main() {
    let play_match = PlayMatch::parse();

    let content = std::fs::read_to_string(play_match.bot_config_path).unwrap();
    let bot_config: BotConfig = toml::from_str(&content).unwrap();

    let uri = play_match
        .grpc_server_url
        .parse()
        .expect("invalid grpc url");

    let channel = Channel::builder(uri).connect().await.unwrap();

    let created_match = create_match(
        channel.clone(),
        play_match.opponent_name,
        play_match.map_name,
    )
    .await
    .unwrap();
    match run_player(bot_config, created_match.player_key, channel).await {
        Ok(()) => (),
        Err(RunPlayerError::RunBotError(err)) => {
            println!("Error running bot: {}", err)
        }
    }
    println!(
        "Match completed. Watch the replay at {}",
        created_match.match_url
    );
    tokio::time::sleep(Duration::from_secs(1)).await;
}

async fn create_match(
    channel: Channel,
    opponent_name: String,
    map_name: Option<String>,
) -> Result<pb::CreateMatchResponse, Status> {
    let mut client = ClientApiServiceClient::new(channel);
    let res = client
        .create_match(Request::new(pb::CreateMatchRequest {
            opponent_name,
            map_name: map_name.unwrap_or_default(),
        }))
        .await;
    res.map(|response| response.into_inner())
}

#[derive(thiserror::Error, Debug)]
enum RunPlayerError {
    #[error("error running bot")]
    RunBotError(std::io::Error),
}

async fn run_player(
    bot_config: BotConfig,
    player_key: String,
    channel: Channel,
) -> Result<(), RunPlayerError> {
    let mut client = ClientApiServiceClient::with_interceptor(channel, |mut req: Request<()>| {
        let player_key: MetadataValue<_> = player_key.parse().unwrap();
        req.metadata_mut().insert("player_key", player_key);
        Ok(req)
    });

    let mut bot_process = Bot {
        working_dir: PathBuf::from(
            bot_config
                .working_directory
                .unwrap_or_else(|| ".".to_string()),
        ),
        argv: bot_config.command.to_argv(),
    }
    .spawn_process();

    let (tx, rx) = mpsc::unbounded_channel();
    let mut stream = client
        .connect_player(UnboundedReceiverStream::new(rx))
        .await
        .unwrap()
        .into_inner();
    while let Some(message) = stream.message().await.unwrap() {
        match message.server_message {
            Some(pb::PlayerApiServerMessageType::ActionRequest(req)) => {
                let moves = bot_process
                    .communicate(&req.content)
                    .await
                    .map_err(RunPlayerError::RunBotError)?;
                let action = pb::PlayerAction {
                    action_request_id: req.action_request_id,
                    content: moves.as_bytes().to_vec(),
                };
                let msg = pb::PlayerApiClientMessage {
                    client_message: Some(pb::PlayerApiClientMessageType::Action(action)),
                };
                tx.send(msg).unwrap();
            }
            _ => {} // pass
        }
    }

    Ok(())
}
