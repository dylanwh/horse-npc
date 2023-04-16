use std::sync::Arc;
use eyre::Result;
use serenity::model::prelude::Ready;
use serenity::prelude::*;
use rusqlite::{params};
use async_openai::{
    types::{ChatCompletionRequestMessageArgs, CreateChatCompletionRequestArgs},
};

// const SYSTEM_PROMPT: &str = include_str!("../system_prompt.txt");

#[derive(Debug)]
struct Message {
    id: i64,
    role: String,
    content: String,
}

struct Handler {
    conn: Arc<tokio_rusqlite::Connection>,
    openai: Arc<async_openai::Client>,
}

impl Handler {
    async fn new() -> Result<Self> {
        let conn = Arc::new(tokio_rusqlite::Connection::open("horse.db").await?);
        // initialize database
        conn.call(move |conn| {
            conn.execute(
                "CREATE TABLE IF NOT EXISTS history (
                    id INTEGER PRIMARY KEY,
                    role VARCHAR(255) NOT NULL,
                    content VARCHAR NOT NULL
                )",
                params![],
            )
        }).await?;
        let openai = Arc::new(async_openai::Client::new().with_api_key(get_openai_key()?));
        Ok(Self {conn, openai})
    }

    async fn reply(&self, message: String) -> Result<String> {
        // read in system_prompt.txt
        let prompt = tokio::fs::read_to_string("system_prompt.txt").await?;

        let mut messages = vec![];
        messages.push(
            ChatCompletionRequestMessageArgs::default()
                .role(async_openai::types::Role::System)
                .content(prompt.clone())
                .build()?
        );

        self.add_message("User", message).await?;
        let history = self.history(10).await?;
        log::info!("begin history");
        for message in history {
            log::info!("context: {}: {}", message.role, message.content);
            let role = match message.role.as_str() {
                "User" => Ok(async_openai::types::Role::User),
                "Assistant" => Ok(async_openai::types::Role::Assistant),
                _ => Err(eyre::eyre!("Invalid role: {}", message.role)),
            }?;
            messages.push(
                ChatCompletionRequestMessageArgs::default()
                    .role(role)
                    .content(message.content)
                    .build()?
            );
        }
        log::info!("end history");

        let request = CreateChatCompletionRequestArgs::default()
            .max_tokens(1024u16)
            .model("gpt-3.5-turbo")
            .temperature(0.8)
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
                self.add_message("Assistant", content).await?;
            }
            else {
                log::warn!("Unexpected choice: {:?}", choice);
            }
        }

        if let Some(reply) = reply {
            Ok(reply)
        }
        else {
            Err(eyre::eyre!("No reply found"))
        }
    }

    async fn add_message<S>(&self, role: S, message: String) -> Result<()>
    where
        S: AsRef<str>,
    {
        let role = role.as_ref().to_string();
        self.conn.call(move |conn| {
            conn.execute(
                "INSERT INTO history (role, content) VALUES (?1, ?2)",
                params![role, message],
            )
        }).await?;

        Ok(())
    }

    async fn history(&self, limit: usize) -> Result<Vec<Message>> {
        let messages = self.conn.call(move |conn| {
            let mut stmt = conn.prepare("SELECT id, role, content FROM history ORDER BY id DESC LIMIT ?1")?;
            let rows = stmt.query_map(params![limit], |row| {
                Ok(Message {
                    id: row.get(0)?,
                    role: row.get(1)?,
                    content: row.get(2)?,
                })
            })?;

            let mut messages: Vec<Message> = Vec::new();
            for row in rows {
                messages.push(row?);
            }
            // sort by id ascending
            messages.sort_by(|a, b| a.id.cmp(&b.id));

            Ok::<_, rusqlite::Error>(messages)
        }).await?;

        Ok(messages)
    }
}

#[serenity::async_trait]
impl EventHandler for Handler {
    async fn message(&self, context: Context, msg: serenity::model::channel::Message) {
        let content = msg
            .content
            .replace("<@1096903557929779300>", "HorseNPC")
            .trim()
            .to_owned();
        // remove HorseNPC from beginning of message
        let content = if content.starts_with("HorseNPC") {
            content[8..].trim().to_owned()
        } else {
            content
        };
        if content.is_empty() {
            return;
        }
        if msg.author.bot {
            return;
        }
        log::info!("{}: {}", msg.author.name, content);
        let mentioned = msg.mentions_me(&context).await.unwrap_or(false);
        if mentioned {
            if let Ok(typing) = msg.channel_id.start_typing(&context.http) {
                let reply = self.reply(content).await.expect("Failed to reply");
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
        | GatewayIntents::MESSAGE_CONTENT;

    let mut client = Client::builder(&get_discord_token()?, intents)
        .event_handler(Handler::new().await?)
        .await?;

    log::info!("Starting client...");

    client.start().await?;

    Ok(())
}

#[allow(dead_code)]
fn get_openai_key() -> Result<String> {
    let key = std::env::var("OPENAI_KEY")?;
    Ok(key)
}

#[allow(dead_code)]
fn get_discord_token() -> Result<String> {
    // get env
    let token = std::env::var("DISCORD_TOKEN")?;
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_reply() -> Result<()> {
        let handler = Handler::new().await?;
        handler.add_message("User", "Hello".to_owned()).await?;
        let history = handler.history(10).await?;
        assert_eq!(history.len(), 1);
        Ok(())
    }
}
