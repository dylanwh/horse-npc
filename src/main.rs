mod db;

use async_openai::types::{
    ChatCompletionRequestMessage, CreateChatCompletionRequestArgs,
};
use bimap::BiMap;
use eyre::{ContextCompat, Result};
use serenity::model::prelude::Message;
use serenity::{
    model::{channel::Channel, prelude::Ready},
    prelude::*,
};
use std::{collections::HashMap, path::PathBuf, sync::Arc};
// use tiktoken_rs::async_openai::get_chat_completion_max_tokens;
// use minijinja::EnvirVonment;

const HORSE_MODERATION_RESPONSES: &str = include_str!("../moderation_responses.txt");

struct Handler {
    schema: db::Schema,
    openai: Arc<async_openai::Client>,
    mentions: Arc<Mutex<BiMap<String, String>>>,
    model: String,
    max_tokens: u16,
}

impl Handler {
    async fn new() -> Result<Self> {
        let schema = db::Schema::new(Some(PathBuf::from("horse.db"))).await?;
        let openai = Arc::new(async_openai::Client::new().with_api_key(get_openai_key()?));
        let mentions = Arc::new(Mutex::new(BiMap::new()));

        Ok(Self {
            schema,
            openai,
            mentions,
            model: "gpt-3.5-turbo".to_owned(),
            max_tokens: 256,
        })
    }

    // async fn parse_message(&self, context: &Context, message: &Message) -> Result<()> {
    //     if message.author.bot {
    //         return Ok(());
    //     }
    //     let messages = vec![
    //         ChatCompletionRequestMessageArgs::default()
    //             .role
    //             .text("Hello, how are you?")
    //             .build()?,
    //         ChatCompletionRequestMessageArgs::default()
    //             .text("I am doing well, thank you.")
    //             .build()?,
    //         ChatCompletionRequestMessageArgs::default()
    //             .text("That is good to hear.")
    //             .build()?,
    //     ];
    //     ]
    //
    //
    //     Ok(())
    // }

    // it actually works out pretty good just leaving the discord references in the message,
    // though I worry since chatgpt isn't good with long numbers it may mix up people eventually. That could actually be amusing though.
    #[allow(dead_code)]
    async fn replace_user_mentions(&self, context: &Context, message: &Message) -> Result<String> {
        let mut content = message.content.clone();
        let mentions = self.mentions.lock().await;
        for mention in message.mentions.iter() {
            let user = mention.id.to_user(&context).await?;
            let mention = format!("<@{}>", user.id);
            if let Some(nickname) = mentions.get_by_left(&mention) {
                content = content.replace(&mention, nickname);
            } else {
                let guild = message.guild(&context).context("No guild found")?;
                let member = guild.member(context, user.id).await?;
                let nickname = member.nick.unwrap_or(user.name).to_owned();
                content = content.replace(&mention.as_str(), &nickname);
            }
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

    async fn current_conversation(
        &self,
        context: &Context,
        message: &Message,
    ) -> Result<db::Conversation> {
        let channel = message.channel_id.to_channel(&context).await?;
        let name = match channel {
            Channel::Guild(g) => format!("#{}", g.name),
            Channel::Private(p) => format!("{}", p.recipient.name),
            _ => format!("unknown"),
        };

        Ok(self.schema.new_conversation(name).await?)
    }

    async fn current_system_prompt(&self, context: &Context, message: &Message) -> Result<String> {
        let now = chrono::Local::now();
        let date = now.format("Today is %A, the %e of %B, %Y. The tine is %I:%M %p");
        let user = message.author.id.to_user(&context).await?;
        let channel = message.channel_id.to_channel(&context).await?;
        let discord_name = message
            .guild_id
            .and_then(|g| g.name(&context))
            .unwrap_or_else(|| "something or other".to_string());

        let channel_info = match channel {
            Channel::Guild(g) => format!(
                r##"in a channel named {}. The topic is: "{}""##,
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

    #[allow(dead_code, unused_variables)]
    async fn get_channel_messages(&self, context: &Context, channel_id: u64) -> Result<Vec<String>> {
        let channel = context
                .http
                .get_channel(channel_id)
                .await?;
        let messages = vec![];

        Ok(messages)
    }

    async fn reply(
        &self,
        conversation: db::Conversation,
        system_prompt: String,
        message: String,
    ) -> Result<String> {
        if self.must_moderate(&message).await? {
            return Ok(random_moderation_response());
        }

        let system_prompt = db::Message {
            id: 0,
            conversation,
            role: db::Role::System,
            content: system_prompt,
        };

        self.schema
            .add_message(conversation, db::Role::User, message)
            .await?;

        let mut messages = self
            .schema
            .history(conversation)
            .await?
            .iter()
            .map(|m| Ok(m.try_into()?))
            .collect::<Result<Vec<ChatCompletionRequestMessage>>>()?;

        messages.insert(messages.len() - 1, system_prompt.try_into()?);

        // get_chat_completion_max_tokens(

        let request = CreateChatCompletionRequestArgs::default()
            .max_tokens(self.max_tokens)
            .model(&self.model)
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
                    .add_message(conversation, db::Role::Assistant, content)
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
    async fn message(&self, context: Context, msg: Message) {
        if msg.author.bot {
            return;
        }
        log::info!("Message: {}", msg.content);
        let mentioned = msg.mentions_me(&context).await.unwrap_or(false);
        let dm = msg.is_private();

        if mentioned || dm {
            let conversation = self
                .current_conversation(&context, &msg)
                .await
                .expect("Failed to get conversation");
            if let Ok(typing) = msg.channel_id.start_typing(&context.http) {
                let system_prompt = self
                    .current_system_prompt(&context, &msg)
                    .await
                    .expect("failed to get system prompt");
                let reply = self
                    .reply(conversation, system_prompt, msg.content.clone())
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
    dotenv::dotenv().ok();
    env_logger::init();

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
