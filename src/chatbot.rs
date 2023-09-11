use crate::{
    helpers::OpenAIHelpers,
    schema::{Conversation, Database, Message, Role},
};
use async_openai::{config::OpenAIConfig, types::CreateChatCompletionRequestArgs};
use async_trait::async_trait;
use eyre::{ContextCompat, Result};
use minijinja::value::Value;

use async_openai::types::ChatCompletionFunctions;
use std::sync::Arc;

const DEFAULT_PROMPT: &str = include_str!("default_prompt.jinja");

pub(crate) fn functions() -> Vec<ChatCompletionFunctions> {
    let functions = include_str!("functions.json");
    serde_json::from_str(functions).expect("Failed to parse functions.json")
}

#[async_trait]
pub trait ChatBot {
    type Message;
    type Context;

    fn openai(&self) -> Arc<async_openai::Client<OpenAIConfig>>;
    fn database(&self) -> Arc<Database>;

    async fn conversation(
        &self,
        context: &Self::Context,
        message: &Self::Message,
    ) -> Result<Conversation>;

    async fn message_content(&self, context: &Self::Context, message: &Self::Message) -> Result<String>;

    async fn prompt_vars(&self, context: &Self::Context, message: &Self::Message) -> Result<Value>;
}

#[allow(unused_variables, dead_code)]
pub async fn reply<B>(bot: B, context: &B::Context, message: &B::Message) -> Result<String>
where
    B: ChatBot,
{
    let openai = bot.openai();
    let db = bot.database();
    let conversation = bot.conversation(context, message).await?;
    let content = bot.message_content(context, message).await?;

    if openai.must_moderate(content.clone()).await? {
        return Ok(random_moderation_response());
    }

    db.add_user_message(conversation, content).await?;
    let mut messages = db.history(conversation).await?;

    let env = minijinja::Environment::new();
    let prompt = db
        .get_prompt(conversation)
        .await?
        .unwrap_or_else(|| DEFAULT_PROMPT.to_owned());
    let prompt = env.render_str(&prompt, bot.prompt_vars(context, message).await?)?;
    messages.insert(0, Message::new(Role::System, prompt));

    let request = CreateChatCompletionRequestArgs::default()
        .max_tokens(db.max_tokens(conversation).await?)
        .model(db.model(conversation).await?)
        .temperature(0.5)
        .functions(functions())
        .messages(
            messages
                .iter()
                .map(|m| m.try_into())
                .collect::<Result<Vec<_>, _>>()?,
        )
        .build()?;

    let response = openai.chat().create(request).await?;
    let choice = response
        .choices
        .into_iter()
        .next()
        .wrap_err("No response")?;
    let message: Message = choice.message.clone().try_into()?;
    let content = message.content();
    
    db.add_message(conversation, message).await?;

    Ok(content)
}

const HORSE_MODERATION_RESPONSES: &str = include_str!("../moderation_responses.txt");

fn random_moderation_response() -> String {
    use rand::prelude::IteratorRandom;
    let mut rng = rand::thread_rng();
    HORSE_MODERATION_RESPONSES
        .lines()
        .choose(&mut rng)
        .unwrap_or("Crikey, I'm not sure what to say")
        .to_owned()
}
