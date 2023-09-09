use crate::schema::{Message, Personality, Role, Schema};
use async_openai::types::{
    ChatCompletionRequestMessage, ChatCompletionRequestMessageArgs, CreateChatCompletionRequestArgs,
};
use eyre::{ContextCompat, Result};
use serenity::{
    model::{channel::Channel, prelude::Ready},
    prelude::*,
};
use std::{collections::HashMap, path::PathBuf, sync::Arc};

mod schema;

// currently unused again, the current system prompt is dynamically generated so that HorseNPC can tell time and who is talking to it.
const HORSE_NPC_PROMPT: &str = include_str!("../system_prompt.txt");
const HORSE_MODERATION_RESPONSES: &str = include_str!("../moderation_responses.txt");

struct Handler {
    schema: Schema,
    openai: Arc<async_openai::Client>,
    personalities: Arc<Mutex<HashMap<String, Personality>>>,
}

impl Handler {
    async fn new() -> Result<Self> {
        let schema = Schema::new(Some(PathBuf::from("horse.db"))).await?;
        let openai = Arc::new(async_openai::Client::new().with_api_key(get_openai_key()?));
        let mut personalities = HashMap::new();
        personalities.insert(
            "horse".to_owned(),
            schema.define_personality("horse", HORSE_NPC_PROMPT).await?,
        );
        let personalities = Arc::new(Mutex::new(personalities));

        Ok(Self {
            schema,
            openai,
            personalities,
        })
    }

    // it actually works out pretty good just leaving the discord references in the message,
    // though I worry since chatgpt isn't good with long numbers it may mix up people eventually. That could actually be amusing though.
    #[allow(dead_code)]
    async fn replace_user_mentions(
        &self,
        context: &Context,
        message: &serenity::model::prelude::Message,
    ) -> Result<String> {
        let mut content = message.content.clone();
        for mention in message.mentions.iter() {
            let user = mention.id.to_user(&context).await?;
            // get guild nickname
            let guild = message.guild(&context).context("No guild found")?;
            let member = guild.member(context, user.id).await?;
            let nickname = member.nick.unwrap_or(user.name);
            let mention = format!("<@{}>", user.id);
            content = content.replace(&mention, &nickname);
        }
        Ok(content)
    }

    async fn must_moderate<S>(&self, message: S) -> Result<bool>
    where
        S: AsRef<str>,
    {
        let response = self
            .openai
            .moderations()
            .create(
                async_openai::types::CreateModerationRequestArgs::default()
                    .input(message.as_ref())
                    .build()?,
            )
            .await?;
        eprintln!("Moderation decision: {:?}", response);
        Ok(response.results.iter().any(|r| r.flagged))
    }

    async fn current_system_prompt(
        &self,
        _personality: Personality,
        context: &Context,
        message: &serenity::model::prelude::Message,
    ) -> Result<String> {
        let now = chrono::Local::now();
        let date = now.format("Today is %A, the %e of %B, %Y. It is %I:%M %p");
        let user = message.author.id.to_user(&context).await?;
        let channel = message.channel_id.to_channel(&context).await?;
        let discord_name = message
            .guild_id
            .and_then(|g| g.name(&context))
            .unwrap_or_else(|| "something or other".to_string());

        let channel_info = match channel {
            Channel::Guild(g) => format!(
                "in a channel named {}. The topic is: {}",
                g.name,
                g.topic
                    .as_ref()
                    .unwrap_or(&format!("anything related to {}", g.name))
            ),
            Channel::Private(p) => format!("in a private channel with <@{}>", p.recipient.id),
            _ => format!("You have no idea where you are"),
        };
        let my_id = context.cache.current_user_id();
        let prompt = format!(
            r#"
            {}.
            Your name is <@{}>.
            You are a horse. You speak only in ridiculous horse puns.
            You are on a discord server named {},
            You are talking to <@{}> {}.
        "#,
            date, my_id, discord_name, user.id, channel_info
        )
        .trim()
        .lines()
        .map(|l| l.trim())
        .collect::<Vec<&str>>()
        .join("\n");

        Ok(prompt.to_owned())
    }

    async fn reply(
        &self,
        personality: Personality,
        system_prompt: String,
        message: String,
    ) -> Result<String> {
        if self.must_moderate(&message).await? {
            return Ok(random_moderation_response());
        }

        let system_prompt = Message {
            id: 0,
            personality,
            role: Role::System,
            content: system_prompt,
        };

        self.schema
            .add_message(personality, Role::User, message)
            .await?;

        let mut messages = self
            .schema
            .history(personality, 10)
            .await?
            .iter()
            .map(|m| Ok(m.try_into()?))
            .collect::<Result<Vec<ChatCompletionRequestMessage>>>()?;

        messages.insert(0, system_prompt.try_into()?);

        let request = CreateChatCompletionRequestArgs::default()
            .max_tokens(256u16)
            .model("gpt-3.5-turbo")
            .temperature(0.5)
            .messages(messages)
            .build()?;

        let response = self.openai.chat().create(request).await?;
        let usage = response.usage;
        log::info!("Usage: {:?}", usage);
        let mut reply: Option<String> = None;
        for choice in response.choices {
            if choice.message.role == async_openai::types::Role::Assistant {
                let content = choice.message.content;
                reply = Some(content.clone());
                self.schema
                    .add_message(personality, Role::Assistant, content)
                    .await?;
            } else {
                log::warn!("Unexpected choice: {:?}", choice);
            }
        }

        if let Some(reply) = reply {
            if self.must_moderate(&reply).await? {
                return Ok(random_moderation_response());
            }
            Ok(reply)
        } else {
            Err(eyre::eyre!("No reply found"))
        }
    }
}

#[serenity::async_trait]
impl EventHandler for Handler {
    async fn message(&self, context: Context, msg: serenity::model::channel::Message) {
        if msg.author.bot {
            return;
        }
        let mentioned = msg.mentions_me(&context).await.unwrap_or(false);
        if mentioned {
            let personality = self
                .personalities
                .lock()
                .await
                .get("horse")
                .expect("didn't find horse personality")
                .clone();
            if let Ok(typing) = msg.channel_id.start_typing(&context.http) {
                let system_prompt = self
                    .current_system_prompt(personality, &context, &msg)
                    .await
                    .expect("failed to get system prompt");
                let reply = self
                    .reply(personality, system_prompt, msg.content.clone())
                    .await
                    .expect("Failed to reply");
                log::info!("HorseNPC: {}", reply);
                let _ = typing.stop();
                match msg.channel_id.say(&context, reply).await {
                    Ok(_) => log::info!("Sent horse"),
                    Err(e) => log::error!("Failed to send horse: {}", e),
                }
            }
        }
    }

    async fn ready(&self, _: Context, ready: Ready) {
        log::info!("{} is connected!", ready.user.name);
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();
    dotenv::dotenv().ok();

    log::info!("Starting up...");

    let intents = GatewayIntents::GUILD_MESSAGES
        | GatewayIntents::DIRECT_MESSAGES
        | GatewayIntents::MESSAGE_CONTENT
        | GatewayIntents::GUILDS;

    let mut client = Client::builder(&get_discord_token()?, intents)
        .event_handler(Handler::new().await?)
        .await?;

    log::info!("Starting client...");

    client.start().await?;

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

fn random_moderation_response() -> String {
    let mut rng = rand::thread_rng();
    use rand::prelude::IteratorRandom;
    HORSE_MODERATION_RESPONSES
        .lines()
        .choose(&mut rng)
        .unwrap_or("Crikey, I'm not sure what to say")
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_random_moderation_response() {
        let response = random_moderation_response();
        assert!(!response.is_empty());
    }

    #[cfg(interactive)]
    #[tokio::test]
    async fn test_must_moderate() {
        let handler = Handler::new().await.unwrap();
        let ok = handler
            .must_moderate("Tell me about trees".to_owned())
            .await
            .unwrap();
        assert!(ok);
    }
}
