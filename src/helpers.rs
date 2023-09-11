use async_openai::{types::CreateModerationRequestArgs, Client, config::OpenAIConfig};
use async_trait::async_trait;
use eyre::{eyre, Result};
use serenity::model::prelude::{Guild, Message};

#[async_trait]
pub trait OpenAIHelpers {
    async fn must_moderate(&self, message: String) -> Result<bool>;
}

#[async_trait]
impl OpenAIHelpers for Client<OpenAIConfig> {
    async fn must_moderate(&self, message: String) -> Result<bool> {
        let response = self
            .moderations()
            .create(
                CreateModerationRequestArgs::default()
                    .input(message)
                    .build()?,
            )
            .await?;
        log::info!("Moderation response: {:?}", response);
        Ok(response.results.iter().any(|r| r.flagged))
    }
}


#[async_trait]
pub trait DiscordContextHelpers {
        async fn get_guild(&self,  message: Option<&Message>) -> Result<Guild>;
}


#[async_trait]
impl DiscordContextHelpers for serenity::prelude::Context {
    async fn get_guild(&self,  message: Option<&Message>) -> Result<Guild> {
        if let Some(message) = message {
            if let Some(guild) = message.guild(self) {
                return Ok(guild);
            }
        }
        let guilds = self.http.get_guilds(None, Some(1)).await?;
        if guilds.is_empty() {
            return Err(eyre!("No guilds found"));
        }
        let guild = guilds[0]
            .id
            .to_guild_cached(self)
            .ok_or_else(|| eyre!("Guild not found"))?;

        Ok(guild)
    }
}


