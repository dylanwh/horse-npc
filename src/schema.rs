use async_openai::types::{ChatCompletionRequestMessage, ChatCompletionRequestMessageArgs};
use eyre::{eyre, Result};
use rusqlite::params;
use std::path::PathBuf;
use tokio_rusqlite::Connection;

const SCHEMA_SQL: &str = include_str!("schema.sql");

#[derive(Debug)]
pub struct Message {
    pub id: i64,
    pub personality: Personality,
    pub role: Role,
    pub content: String,
}

#[derive(Debug, Ord, PartialOrd, Eq, PartialEq, Clone, Copy)]
pub struct Personality(i64);

#[derive(Debug, Ord, PartialOrd, Eq, PartialEq, Clone, Copy)]
pub enum Role {
    System,
    User,
    Assistant,
}

impl From<Role> for u8 {
    fn from(role: Role) -> Self {
        match role {
            Role::System => 0,
            Role::User => 1,
            Role::Assistant => 2,
        }
    }
}

impl From<Role> for async_openai::types::Role {
    fn from(role: Role) -> Self {
        match role {
            Role::System => async_openai::types::Role::System,
            Role::User => async_openai::types::Role::User,
            Role::Assistant => async_openai::types::Role::Assistant,
        }
    }
}

impl TryFrom<Message> for ChatCompletionRequestMessage {
    type Error = async_openai::error::OpenAIError;

    fn try_from(message: Message) -> std::result::Result<Self, Self::Error> {
        let role: async_openai::types::Role = message.role.into();
        ChatCompletionRequestMessageArgs::default()
            .role(role)
            .content(message.content)
            .build()
    }
}

impl TryFrom<&Message> for ChatCompletionRequestMessage {
    type Error = async_openai::error::OpenAIError;

    fn try_from(message: &Message) -> std::result::Result<Self, Self::Error> {
        let role: async_openai::types::Role = message.role.into();
        ChatCompletionRequestMessageArgs::default()
            .role(role)
            .content(&message.content)
            .build()
    }
}

impl TryFrom<u8> for Role {
    type Error = eyre::Report;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Role::System),
            1 => Ok(Role::User),
            2 => Ok(Role::Assistant),
            _ => Err(eyre!("Invalid role: {}", value)),
        }
    }
}

pub struct Schema {
    conn: Connection,
}

impl Schema {
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

    pub async fn define_personality<P1, P2>(
        &self,
        personality: P1,
        prompt: P2,
    ) -> Result<Personality>
    where
        P1: AsRef<str>,
        P2: AsRef<str>,
    {
        let rowid = {
            let personality = personality.as_ref().to_string();
            let prompt = prompt.as_ref().to_string();
            self.conn
                .call(move |conn| {
                    conn.execute(
                        "INSERT INTO personalities (name, prompt) VALUES (?1, ?2)
                ON CONFLICT (name) DO UPDATE SET prompt = ?2",
                        params![personality, prompt],
                    )?;
                    let rowid = conn.last_insert_rowid();
                    if rowid == 0 {
                        let mut stmt =
                            conn.prepare("SELECT id FROM personalities WHERE name = ?1")?;
                        let mut rows =
                            stmt.query_map(params![personality], |row| Ok(row.get(0)?))?;
                        let id = if let Some(row) = rows.next() {
                            row?
                        } else {
                            return Err(rusqlite::Error::QueryReturnedNoRows);
                        };

                        Ok(id)
                    } else {
                        Ok(rowid)
                    }
                })
                .await?
        };

        let id = if rowid > 0 {
            Ok(rowid)
        } else {
            Err(eyre!(
                "Failed to insert personality: {}, got row id: {}",
                personality.as_ref(),
                rowid
            ))
        }?;

        Ok(Personality(id))
    }

    #[allow(dead_code)]
    pub async fn get_personality_prompt(&self, personality: Personality) -> Result<String> {
        let personality_id = personality.0;
        let prompt = self
            .conn
            .call(move |conn| {
                let mut stmt = conn.prepare("SELECT prompt FROM personalities WHERE id = ?1")?;
                let mut rows = stmt.query_map(params![personality_id], |row| Ok(row.get(0)?))?;

                if let Some(row) = rows.next() {
                    Ok(row?)
                } else {
                    Err(rusqlite::Error::QueryReturnedNoRows)
                }
            })
            .await?;

        Ok(prompt)
    }

    pub async fn add_message<S>(
        &self,
        personality: Personality,
        role: Role,
        message: S,
    ) -> Result<()>
    where
        S: AsRef<str>,
    {
        let message = message.as_ref().to_string();
        self.conn
            .call(move |conn| {
                let role: u8 = role.into();
                conn.execute(
                    "INSERT INTO history (personality, role, content) VALUES (?1, ?2, ?3)",
                    params![personality.0, role, message],
                )
            })
            .await?;

        Ok(())
    }

    const HISTORY_SQL: &'static str = r#"
        SELECT id, role, content FROM history
        WHERE personality = ?1
        ORDER BY id DESC
        LIMIT ?2
    "#;

    pub async fn history(&self, personality: Personality, limit: usize) -> Result<Vec<Message>> {
        let messages = self
            .conn
            .call(move |conn| {
                let mut stmt = conn.prepare(Self::HISTORY_SQL)?;
                let rows = stmt.query_map(params![personality.0, limit], |row| {
                    let id = row.get(0)?;
                    let role: u8 = row.get(1)?;

                    // meh, this is a bit annoying to convert from a TryFrom error to a rusqlite error.
                    let role: Role = Role::try_from(role)
                        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(e.into()))?;

                    let content = row.get(2)?;

                    Ok(Message {
                        id,
                        personality,
                        role,
                        content,
                    })
                })?;

                let mut messages: Vec<Message> = Vec::new();
                for row in rows {
                    messages.push(row?);
                }
                // sort by id ascending
                messages.sort_by(|a, b| a.id.cmp(&b.id));

                Ok::<_, rusqlite::Error>(messages)
            })
            .await?;

        Ok(messages)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_personalities() {
        let schema = Schema::new(None).await.expect("failed to create schema");
        let personality = schema
            .define_personality("test", "test prompt")
            .await
            .expect("failed to define personality");
        let prompt = schema
            .get_personality_prompt(personality)
            .await
            .expect("failed to get prompt");
        assert_eq!(prompt, "test prompt".to_string());
    }

    #[tokio::test]
    async fn test_roles() {
        let role = Role::System;
        let role: u8 = role.into();
        assert_eq!(role, 0);
        let role: Role = role.try_into().unwrap();
        assert_eq!(role, Role::System);

        let role = Role::User;
        let role: u8 = role.into();
        assert_eq!(role, 1);
        let role: Role = role.try_into().unwrap();
        assert_eq!(role, Role::User);

        let role = Role::Assistant;
        let role: u8 = role.into();
        assert_eq!(role, 2);
        let role: Role = role.try_into().unwrap();
        assert_eq!(role, Role::Assistant);
    }

    #[tokio::test]
    async fn test_history() {
        let schema = Schema::new(None).await.expect("failed to create schema");
        let personality = schema
            .define_personality("test", "test prompt")
            .await
            .expect("failed to define personality");
        let role = Role::System;
        let message = "test message";
        schema
            .add_message(personality, role, message)
            .await
            .expect("failed to add message");
        let messages = schema
            .history(personality, 1)
            .await
            .expect("failed to get history");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, role);
        assert_eq!(messages[0].content, message);
    }
}
