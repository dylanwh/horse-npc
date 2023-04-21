mod db;

use async_openai::types::{
    ChatCompletionRequestMessage, ChatCompletionRequestMessageArgs, CreateChatCompletionRequestArgs,
};
use bimap::BiMap;
use eyre::{ContextCompat, Result};
use itertools::intersperse;
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

    #[allow(dead_code)]
    async fn decode_user_mentions(&self, context: &Context, message: &Message) -> Result<String> {
        let mut content = message.content.clone();
        let mut mentions = self.mentions.lock().await;
        for mention in message.mentions.iter() {
            let user = mention.id.to_user(&context).await?;
            let mention = format!("<@{}>", user.id);
            if let Some(nickname) = mentions.get_by_left(&mention) {
                content = content.replace(&mention, nickname);
            } else {
                let guild = message.guild(&context).context("No guild found")?;
                let member = guild.member(context, user.id).await?;
                let nickname = format!("@{}", member.nick.unwrap_or(user.name).to_owned());
                content = content.replace(&mention.as_str(), &nickname);
                mentions.insert(mention, nickname);
            }
        }
        Ok(content)
    }

    async fn decode_user_mentions_from_str<S>(
        &self,
        context: &Context,
        message: &Message,
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
            let guild = message.guild(&context).context("No guild found")?;
            let member = guild.member(context, user.id).await?;
            let nickname = format!("@{}", member.nick.unwrap_or(user.name).to_owned());
            mentions.insert(mention, nickname);
        }

        let result = re.replace_all(content.as_ref(), |caps: &regex::Captures| {
            let user_id = caps.get(1).map(|m| m.as_str()).unwrap_or("").to_owned();
            let user_id = user_id.parse::<u64>().unwrap_or(0);
            let mention = format!("<@{}>", user_id);
            mentions.get_by_left(&mention).cloned().unwrap_or(mention)
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
            let nickname = caps.get(0).map(|m| m.as_str()).unwrap_or("").to_owned();
            mentions
                .get_by_right(&nickname)
                .cloned()
                .unwrap_or(nickname)
        });
        Ok(result.to_string())
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

        let prompt = self
            .decode_user_mentions_from_str(context, &message, prompt)
            .await?;
        log::info!("system prompt: {}", prompt);
        Ok(prompt)
    }

    async fn reply(
        &self,
        conversation: db::Conversation,
        system_prompt: String,
        content: String,
    ) -> Result<String> {
        if self.must_moderate(&content).await? {
            return Ok(random_moderation_response());
        }

        let system_prompt = db::Message {
            id: 0,
            conversation,
            role: db::Role::System,
            content: system_prompt,
        };

        self.schema
            .add_message(conversation, db::Role::User, content)
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
        let mentioned = msg.mentions_me(&context).await.unwrap_or(false);
        let dm = msg.is_private();

        if mentioned || dm {
            let conversation = self
                .current_conversation(&context, &msg)
                .await
                .expect("Failed to get conversation");
            if let Ok(typing) = msg.channel_id.start_typing(&context.http) {
                let content = msg.content.clone();
                let content = self
                    .decode_user_mentions(&context, &msg)
                    .await
                    .expect("decode_user_mentions");
                let system_prompt = self
                    .current_system_prompt(&context, &msg)
                    .await
                    .expect("failed to get system prompt");
                let reply = self
                    .reply(conversation, system_prompt, content)
                    .await
                    .expect("Failed to reply");
                let reply = self
                    .encode_user_mentions(reply)
                    .await
                    .expect("encode_user_mentions");
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

    #[tokio::test]
    async fn test_encode_user_mentions() {
        let handler = Handler::new().await.unwrap();
        {
            let mut mentions_map = handler.mentions.lock().await;
            mentions_map.insert("@Alice".to_owned(), "<@1234>".to_owned());
            mentions_map.insert("@Bob".to_owned(), "<@5678>".to_owned());
            mentions_map.insert("@Charlie".to_owned(), "<@9012>".to_owned());
        }

        let content = "Hello @Alice, @Bob, and @Charlie!".to_owned();
        let expected_result = "Hello <@1234>, <@5678>, and <@9012>!".to_owned();

        let result = handler.encode_user_mentions(content).await.unwrap();

        assert_eq!(result, expected_result);
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
