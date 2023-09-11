extern crate core;

mod chatbot;
mod helpers;
mod schema;

use async_openai::config::OpenAIConfig;
use async_trait::async_trait;
use bimap::BiMap;
use chatbot::ChatBot;
use clap::Parser;
use eyre::{Context, Result};

use helpers::DiscordContextHelpers;
use itertools::intersperse;
use minijinja::{context, value::Value};
use schema::{Conversation, Database};
use serenity::{
    model::{
        prelude::{Channel, Guild, Message, Ready},
        user::User,
    },
    prelude::{self as discord},
};
use std::{path::PathBuf, sync::Arc};
use tokio::sync::Mutex;
use unicase::UniCase;

// use tiktoken_rs::async_openai::get_chat_completion_max_tokens;

#[derive(Debug, clap::Parser)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[clap(short, long)]
    database: Option<PathBuf>,

    #[clap(subcommand)]
    command: Command,
}

#[derive(Debug, clap::Parser)]
enum Command {
    Run,
    Test,
}

struct DiscordBot {
    database: Arc<Database>,
    openai: Arc<async_openai::Client<OpenAIConfig>>,
    mentions: Arc<Mutex<BiMap<String, UniCase<String>>>>,
}

#[async_trait]
impl ChatBot for &DiscordBot {
    type Message = Message;
    type Context = discord::Context;

    fn openai(&self) -> Arc<async_openai::Client<OpenAIConfig>> {
        self.openai.clone()
    }

    fn database(&self) -> Arc<Database> {
        self.database.clone()
    }

    async fn message_content(
        &self,
        context: &Self::Context,
        message: &Self::Message,
    ) -> Result<String> {
        let content = message.content.clone();
        let content = self
            .decode_user_mentions(context, Some(message), content)
            .await?;

        Ok(content)
    }

    async fn conversation(
        &self,
        context: &Self::Context,
        message: &Self::Message,
    ) -> Result<Conversation> {
        let channel = message.channel_id.to_channel(&context).await?;
        let name = match channel {
            Channel::Guild(g) => {
                // determine if thread or regular channel
                if let Some(parent) = g.parent_id {
                    let parent = parent.to_channel(&context).await?;
                    if let Channel::Guild(parent) = parent {
                        format!("#{}:{}", parent.name, g.name)
                    } else {
                        format!("#{}", g.name)
                    }
                } else {
                    format!("#{}", g.name)
                }
            }
            Channel::Private(p) => p.recipient.name.to_string(),
            _ => "unknown".to_string(),
        };
        self.database().find_conversation(name).await
    }

    async fn prompt_vars(&self, context: &Self::Context, message: &Self::Message) -> Result<Value> {
        let now = chrono::Local::now();
        let date = now
            .format("Today is %A, the %e of %B, %Y. The time is %I:%M %p")
            .to_string();
        let guild = context.get_guild(Some(message)).await?;
        let user = message.author.id.to_user(&context).await?;
        let bot = context.cache.current_user_id().to_user(&context).await?;
        let user_nick = get_nickname(context, &guild, &user).await?;
        let bot_nick = get_nickname(context, &guild, &bot).await?;
        let channel = message.channel_id.to_channel(&context).await?;
        let server_name = message.guild_id.and_then(|g| g.name(context));
        let (channel_name, channel_topic) = match channel {
            Channel::Guild(g) => (Some(g.name), g.topic),
            _ => (None, None),
        };

        Ok(context! {
            user_nick => format!("@{}", user_nick),
            bot_nick => format!("@{}", bot_nick),
            date,
            server_name,
            channel_name,
            channel_topic,
        })
    }
}

async fn get_nickname(context: &discord::Context, guild: &Guild, user: &User) -> Result<String> {
    let member = guild.member(context, user.id).await?;
    Ok(member.nick.unwrap_or(user.clone().name).to_owned())
}

impl DiscordBot {
    async fn new(db_path: Option<PathBuf>) -> Result<Self> {
        let schema = Arc::new(Database::new(db_path).await?);
        let config = OpenAIConfig::new().with_api_key(get_openai_key()?);
        let openai = Arc::new(async_openai::Client::with_config(config));
        let mentions = Arc::new(Mutex::new(BiMap::new()));

        Ok(Self {
            database: schema,
            openai,
            mentions,
        })
    }

    async fn decode_user_mentions<S>(
        &self,
        context: &discord::Context,
        message: Option<&Message>,
        content: S,
    ) -> Result<String>
    where
        S: AsRef<str>,
    {
        let re = regex::Regex::new(r"<@(\d+)>")?;
        let mut mentions = self.mentions.lock().await;

        // iterate over all regex matches
        for caps in re.captures_iter(content.as_ref()) {
            // get the user id
            let Some(user_id) = caps.get(1).map(|m| m.as_str()) else { continue };
            let user_id = user_id.parse::<u64>()?;
            let mention = format!("<@{}>", user_id);
            if mentions.contains_left(&mention) {
                continue;
            }
            let user = context.http.get_user(user_id).await?;
            let guild = context.get_guild(message).await?;
            let member = guild.member(context, user.id).await?;
            let nickname = format!("@{}", member.nick.unwrap_or(user.name).to_owned());
            mentions.insert(mention, UniCase::new(nickname));
        }

        let result = re.replace_all(content.as_ref(), |caps: &regex::Captures| {
            let user_id = caps.get(1).map(|m| m.as_str()).unwrap_or("").to_owned();
            let user_id = user_id.parse::<u64>().unwrap_or(0);
            let mention = format!("<@{}>", user_id);

            mentions
                .get_by_left(&mention)
                .cloned()
                .map(|s| s.to_string())
                .unwrap_or(mention.clone())
        });

        Ok(result.to_string())
    }

    async fn encode_user_mentions<S>(&self, content: S) -> Result<String>
    where
        S: AsRef<str>,
    {
        let mentions = self.mentions.lock().await;

        let pattern = intersperse(
            mentions.right_values().map(|s| regex::escape(s)),
            "|".to_owned(),
        )
        .collect::<String>();

        let re = regex::Regex::new(&pattern)?;
        let result = re.replace_all(content.as_ref(), |caps: &regex::Captures| {
            let nickname_string = caps.get(0).map(|m| m.as_str()).unwrap_or("").to_owned();
            let nickname = UniCase::new(nickname_string.clone());
            mentions
                .get_by_right(&nickname)
                .cloned()
                .unwrap_or(nickname_string)
                .to_string()
        });
        Ok(result.to_string())
    }

    #[allow(dead_code, unused_variables)]
    async fn get_channel_messages(
        &self,
        context: &discord::Context,
        channel_id: u64,
    ) -> Result<Vec<String>> {
        let channel = context.http.get_channel(channel_id).await?;
        let messages = vec![];

        Ok(messages)
    }

    // this is called by EventHandler::message, but it can return a Result.
    // any errors will be reported to the user.
    async fn message_hook(&self, context: discord::Context, msg: Message) -> Result<()> {
        let mentioned = msg.mentions_me(&context).await.unwrap_or(false);
        let dm = msg.is_private();

        if mentioned || dm {
            if let Ok(typing) = msg.channel_id.start_typing(&context.http) {
                let reply = chatbot::reply(self, &context, &msg).await?;
                let reply = self
                    .encode_user_mentions(reply)
                    .await
                    .wrap_err("encode_user_mentions")?;
                log::info!("HorseNPC: {}", reply);
                let _ = typing.stop();
                match msg.channel_id.say(&context, reply).await {
                    Ok(_) => log::info!("Sent horse"),
                    Err(e) => log::error!("Failed to send horse: {}", e),
                }
            }
        }

        Ok(())
    }
}

#[serenity::async_trait]
impl discord::EventHandler for DiscordBot {
    async fn message(&self, context: discord::Context, msg: Message) {
        if msg.author.bot {
            return;
        }

        if let Err(e) = self.message_hook(context, msg).await {
            log::error!("Error: {}", e);
        }
    }

    async fn ready(&self, _: discord::Context, ready: Ready) {
        log::info!("{} is connected!", ready.user.name);
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> Result<()> {
    dotenv::dotenv().ok();
    env_logger::init();

    let args: Args = Args::parse();
    match args.command {
        Command::Run => run(args).await,
        Command::Test => test(args).await,
    }
}

async fn run(_args: Args) -> Result<()> {
    log::info!("Starting up...");
    let bot = DiscordBot::new(None).await?;

    let intents = discord::GatewayIntents::GUILD_MESSAGES
        | discord::GatewayIntents::DIRECT_MESSAGES
        | discord::GatewayIntents::MESSAGE_CONTENT
        | discord::GatewayIntents::GUILDS;

    let mut client = discord::Client::builder(&get_discord_token()?, intents)
        .event_handler(bot)
        .await?;

    log::info!("Starting client...");

    client.cache_and_http.cache.set_max_messages(2000);
    client.start().await?;

    Ok(())
}

struct TestBot {
    openai: Arc<async_openai::Client<OpenAIConfig>>,
    database: Arc<Database>,
}

#[async_trait]
impl ChatBot for TestBot {
    type Message = String;
    type Context = ();

    fn openai(&self) -> Arc<async_openai::Client<OpenAIConfig>> {
        self.openai.clone()
    }

    fn database(&self) -> Arc<Database> {
        self.database.clone()
    }

    async fn message_content(
        &self,
        _context: &Self::Context,
        message: &Self::Message,
    ) -> Result<String> {
        Ok(message.to_owned())
    }

    async fn conversation(
        &self,
        _context: &Self::Context,
        _message: &Self::Message,
    ) -> Result<Conversation> {
        self.database.find_conversation("test").await
    }

    async fn prompt_vars(
        &self,
        _context: &Self::Context,
        _message: &Self::Message,
    ) -> Result<Value> {
        Ok(context! {
            user_nick => "@dylan",
            bot_nick => "@HorseNPC",
            date => "Today is Monday, the 1st of January, 2021. The time is 12:00 PM",
            server_name => "Test Server",
            channel_name => "#test",
            channel_topic => "This is a test channel",
        })
    }
}

async fn test(_args: Args) -> Result<()> {
    let config = OpenAIConfig::new().with_api_key(get_openai_key()?);
    let openai = Arc::new(async_openai::Client::with_config(config));
    let database = Arc::new(Database::new(None).await?);
    let bot = TestBot { openai, database };
    let message = "Hello, world!".to_owned();
    let reply = chatbot::reply(bot, &(), &message).await?;
    println!("{}", reply);

    Ok(())
}

fn get_openai_key() -> Result<String> {
    let key = std::env::var("OPENAI_KEY")?;
    Ok(key)
}

fn get_discord_token() -> Result<String> {
    // get env
    let token = std::env::var("DISCORD_TOKEN")?;
    Ok(token)
}
