mod minecraft;

use futures::FutureExt;
use serde::Deserialize;
use serenity::{
    async_trait,
    model::{
        gateway::Ready,
        id::{ChannelId, UserId},
        voice::VoiceState,
    },
    prelude::*,
};
use std::{collections::HashSet, fs::File, io::BufReader, sync::Arc, sync::Mutex};
use tokio::{
    signal::unix::{signal, SignalKind},
    sync,
};

#[derive(Deserialize)]
pub struct Config {
    discord: DiscordConfig,
    server: ServerConfig,
}

#[derive(Deserialize)]
pub struct DiscordConfig {
    token: String,
    channel: ChannelId,
}

#[derive(Deserialize)]
pub struct ServerConfig {
    version: String,
    jvm_arguments: Vec<String>,
}

struct VoiceTracker {
    channel: ChannelId,
    users: HashSet<UserId>,
    tx: sync::watch::Sender<bool>,
}

impl VoiceTracker {
    fn new(channel: ChannelId, tx: tokio::sync::watch::Sender<bool>) -> Self {
        Self {
            channel,
            users: HashSet::new(),
            tx,
        }
    }

    fn handle(&mut self, state: VoiceState) {
        let was_empty = self.users.is_empty();
        if state.channel_id == Some(self.channel) {
            self.users.insert(state.user_id);
        } else {
            self.users.remove(&state.user_id);
        }
        let is_empty = self.users.is_empty();

        if was_empty && !is_empty {
            drop(self.tx.send(true));
        } else if is_empty && !was_empty {
            drop(self.tx.send(false));
        }
    }
}

struct Handler {
    tracker: Arc<Mutex<VoiceTracker>>,
}
#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, _: Context, ready: Ready) {
        log::info!("Connected as {}", ready.user.name);
    }

    async fn voice_state_update(
        &self,
        _: Context,
        _: Option<serenity::model::id::GuildId>,
        _old: Option<VoiceState>,
        new: VoiceState,
    ) {
        self.tracker.lock().unwrap().handle(new);
    }
}

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("mcd=debug")).init();
    log::info!("mcd starting...");

    let config: Config =
        serde_json::from_reader(BufReader::new(File::open("config.json").unwrap())).unwrap();

    let server_task = minecraft::spawn_server_task(config.server);

    // Create a new instance of the Client, logging in as a bot. This will
    // automatically prepend your bot token with "Bot ", which is a requirement
    // by Discord for bot users.
    let mut client = Client::builder(&config.discord.token)
        .event_handler(Handler {
            tracker: Arc::new(Mutex::new(VoiceTracker::new(
                config.discord.channel,
                server_task.should_run,
            ))),
        })
        .await
        .expect("Err creating client");

    let shard_manager = client.shard_manager.clone();
    tokio::spawn(async move {
        // Wait for a ctrl+c or SIGTERM
        let signals = [SignalKind::interrupt(), SignalKind::terminate()];

        futures::future::select_all(
            IntoIterator::into_iter(signals)
                .map(|s| signal(s).expect("Could not register signal handler"))
                .map(|mut s| async move { s.recv().await }.boxed()),
        )
        .await;
        shard_manager.lock().await.shutdown_all().await;
    });

    // Finally, start a single shard, and start listening to events.
    //
    // Shards will automatically attempt to reconnect, and will perform
    // exponential backoff until it reconnects.
    if let Err(why) = client.start().await {
        log::error!("Client error: {:?}", why);
    }

    log::info!("Shutting down...");
    drop(server_task.join_handle.await);
}
