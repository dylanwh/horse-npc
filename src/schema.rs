mod model;

pub use model::{Conversation, Message, Role};

use eyre::Result;
use rusqlite::params;
use std::path::PathBuf;
use tokio_rusqlite::Connection;

const SCHEMA_SQL: &str = include_str!("schema.sql");

pub struct Database {
    conn: Connection,
}

impl Database {
    pub async fn new(path: Option<PathBuf>) -> Result<Self> {
        let conn = if let Some(path) = path {
            Connection::open(path).await
        } else {
            Connection::open_in_memory().await
        }?;

        conn.call(move |conn| {
            conn.execute_batch(SCHEMA_SQL)?;
            Ok(())
        })
        .await?;

        Ok(Self { conn })
    }

    pub async fn set_prompt<S>(&self, conversation: Conversation, text: S) -> Result<()>
    where
        S: AsRef<str>
    {
        let text = text.as_ref().to_owned();

        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO conversation (id, prompt) VALUES (?1, ?2)
                ON CONFLICT (id) DO UPDATE SET prompt = ?2",
                    params![conversation.0, text],
                )?;
                Ok(())
            })
            .await?;

        Ok(())
    }


    pub async fn get_prompt(&self, conversation: Conversation) -> Result<Option<String>>
    {
        let text = self
            .conn
            .call(move |conn| {
                let mut stmt = conn.prepare("SELECT prompt FROM conversation WHERE id = ?1")?;
                let mut rows = stmt.query_map(params![conversation.0], |row| row.get(0))?;
                let text = if let Some(row) = rows.next() {
                    row?
                } else {
                    return Err(rusqlite::Error::QueryReturnedNoRows);
                };

                Ok(text)
            })
            .await?;
        Ok(text)
    }

    pub async fn find_conversation<S>(&self, name: S) -> Result<Conversation>
    where
        S: AsRef<str>,
    {
        let name = name.as_ref().to_owned();
        let conversation: Conversation = self
            .conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO conversation (name) VALUES (?1) ON CONFLICT (name) DO NOTHING",
                    params![name],
                )?;
                let mut stmt =
                    conn.prepare("SELECT id FROM conversation WHERE name = ?1 LIMIT 1")?;
                let mut rows =
                    stmt.query_map(params![name], |row| Ok(Conversation(row.get(0)?)))?;
                let conversation = if let Some(row) = rows.next() {
                    row?
                } else {
                    return Err(rusqlite::Error::QueryReturnedNoRows);
                };

                Ok(conversation)
            })
            .await?;
        Ok(conversation)
    }

    pub async fn add_user_message<S>(&self, conversation: Conversation, content: S) -> Result<()>
    where
        S: AsRef<str>,
    {
        let content = content.as_ref().to_owned();
        let message = Message::new(Role::User, content);

        self.add_message(conversation, message).await
    }

    pub async fn add_assistant_message<S>(
        &self,
        conversation: Conversation,
        content: S,
    ) -> Result<()>
    where
        S: AsRef<str>,
    {
        let content = content.as_ref().to_owned();
        let message = Message::new(Role::Assistant, content);

        self.add_message(conversation, message).await
    }

    #[allow(unused)]
    pub async fn add_message(&self, conversation: Conversation, message: Message) -> Result<()> {
        let message = serde_json::to_string(&message)?;

        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO history (conversation, message) VALUES (?1, ?2)",
                    params![conversation.0, message],
                )?;
                Ok(())
            })
            .await?;
        Ok(())
    }

    const HISTORY_SQL: &'static str = r#"
        SELECT id, message FROM history
        WHERE conversation = ?1
        ORDER BY id ASC
    "#;

    #[allow(unused)]
    pub async fn history(&self, conversation: Conversation) -> Result<Vec<Message>> {
        let messages = self
            .conn
            .call(move |conn| {
                let mut stmt = conn.prepare(Self::HISTORY_SQL)?;
                let mut rows = stmt.query_map(params![conversation.0], |row| {
                    let id: i64 = row.get(0)?;
                    let message: String = row.get(1)?;
                    let message: Message = serde_json::from_str(&message).map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(
                            0,
                            rusqlite::types::Type::Text,
                            Box::new(e),
                        )
                    })?;
                    Ok(message)
                })?;

                rows.collect::<Result<Vec<Message>, rusqlite::Error>>()
            })
            .await?;

        Ok(messages)
    }

    pub async fn model(&self, conversation: Conversation) -> Result<String> {
        let model: String = self
            .conn
            .call(move |conn| {
                let mut stmt = conn.prepare("SELECT model FROM conversation WHERE id = ?1")?;
                let mut rows = stmt.query_map(params![conversation.0], |row| row.get(0))?;
                let model = if let Some(row) = rows.next() {
                    row?
                } else {
                    return Err(rusqlite::Error::QueryReturnedNoRows);
                };

                Ok(model)
            })
            .await?;
        Ok(model)
    }

    pub async fn max_tokens(&self, conversation: Conversation) -> Result<u16> {
        let max_tokens: u16 = self
            .conn
            .call(move |conn| {
                let mut stmt = conn.prepare("SELECT max_tokens FROM conversation WHERE id = ?1")?;
                let mut rows = stmt.query_map(params![conversation.0], |row| row.get(0))?;
                let max_tokens = if let Some(row) = rows.next() {
                    row?
                } else {
                    return Err(rusqlite::Error::QueryReturnedNoRows);
                };

                Ok(max_tokens)
            })
            .await?;
        Ok(max_tokens)
    }
}

#[cfg(test)]
mod tests {
    use async_openai::types::ChatCompletionResponseMessage;

    use super::*;

    #[tokio::test]
    async fn test_conversation() {
        let db = Database::new(None).await.expect("failed to create schema");
        let c1 = db
            .find_conversation("test")
            .await
            .expect("failed to find conversation");
        let c2 = db
            .find_conversation("test")
            .await
            .expect("failed to find conversation");
        assert_eq!(c1, c2);
    }

    #[tokio::test]
    async fn test_history() {
        let db = Database::new(None).await.expect("failed to create db");
        let conversation = db
            .find_conversation("test")
            .await
            .expect("failed to define conversation");
        let message = Message::new(Role::System, "test");
        db
            .add_message(conversation, message)
            .await
            .expect("failed to add message");
        let message = ChatCompletionResponseMessage {
            role: async_openai::types::Role::Assistant,
            content: None,
            function_call: Some(async_openai::types::FunctionCall {
                name: "react".to_owned(),
                arguments: "{\n  \"reaction_name\": \":thinking:\"\n}".to_owned(),
            }),
        };
        db
            .add_message(conversation, message.try_into().unwrap())
            .await
            .expect("failed to add message");

        let messages = db
            .history(conversation)
            .await
            .expect("failed to get history");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role(), Role::System);
        assert_eq!(messages[1].role(), Role::Assistant);
    }
}
