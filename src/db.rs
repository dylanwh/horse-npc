use async_openai::types::{ChatCompletionRequestMessage, ChatCompletionRequestMessageArgs};
use eyre::{eyre, Result};
use rusqlite::params;
use std::path::PathBuf;
use tokio_rusqlite::Connection;

const SCHEMA_SQL: &str = include_str!("schema.sql");

#[derive(Debug)]
pub struct Message {
    pub id: i64,
    pub conversation: Conversation,
    pub role: Role,
    pub content: String,
}

#[derive(Debug, Ord, PartialOrd, Eq, PartialEq, Clone, Copy)]
pub struct Conversation(u64);

#[derive(Debug, Ord, PartialOrd, Eq, PartialEq, Clone, Copy)]
pub struct Template(u64);

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

    pub async fn set_template<N, T>(&self, name: N, text: T) -> Result<()>
    where
        N: AsRef<str>,
        T: AsRef<str>,
    {
        let name = name.as_ref().to_owned();
        let text = text.as_ref().to_owned();

        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO template (name, content) VALUES (?1, ?2)
                ON CONFLICT (name) DO UPDATE SET content = ?2",
                    params![name, text],
                )?;
                Ok(())
            })
            .await?;

        Ok(())
    }

    pub async fn get_template<S>(&self, name: S) -> Result<String>
    where
        S: AsRef<str>,
    {
        let name = name.as_ref().to_owned();
        let text = self
            .conn
            .call(move |conn| {
                let mut stmt = conn.prepare("SELECT text FROM template WHERE name = ?1")?;
                let mut rows = stmt.query_map(params![name], |row| Ok(row.get(0)?))?;
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

    pub async fn find_conversation<S>(&self, conversation: S) -> Result<Conversation>
    where
        S: AsRef<str>,
    {
        let rowid = {
            let conversation = conversation.as_ref().to_string();
            self.conn
                .call(move |conn| {
                    conn.execute(
                        "INSERT INTO conversation (name) VALUES (?1) ON CONFLICT (name) DO NOTHING",
                        params![conversation],
                    )?;
                    let mut stmt = conn.prepare("SELECT id FROM conversation WHERE name = ?1")?;
                    let mut rows = stmt.query_map(params![conversation], |row| Ok(row.get(0)?))?;
                    let id = if let Some(row) = rows.next() {
                        row?
                    } else {
                        return Err(rusqlite::Error::QueryReturnedNoRows);
                    };

                    Ok(id)
                })
                .await?
        };

        let id = if rowid > 0 {
            Ok(rowid)
        } else {
            Err(eyre!(
                "Failed to insert conversation: {}, got row id: {}",
                conversation.as_ref(),
                rowid
            ))
        }?;

        Ok(Conversation(id))
    }

    pub async fn add_message<S>(
        &self,
        conversation: Conversation,
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
                    "INSERT INTO history (conversation, role, content) VALUES (?1, ?2, ?3)",
                    params![conversation.0, role, message],
                )
            })
            .await?;

        Ok(())
    }

    const HISTORY_SQL: &'static str = r#"
        SELECT id, role, content FROM history
        WHERE conversation = ?1
        ORDER BY id DESC
    "#;

    pub async fn history(&self, conversation: Conversation) -> Result<Vec<Message>> {
        let messages = self
            .conn
            .call(move |conn| {
                let mut stmt = conn.prepare(Self::HISTORY_SQL)?;
                let rows = stmt.query_map(params![conversation.0], |row| {
                    let id = row.get(0)?;
                    let role: u8 = row.get(1)?;

                    // meh, this is a bit annoying to convert from a TryFrom error to a rusqlite error.
                    let role: Role = Role::try_from(role)
                        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(e.into()))?;

                    let content = row.get(2)?;

                    Ok(Message {
                        id,
                        conversation,
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
    async fn test_conversation() {
        let schema = Schema::new(None).await.expect("failed to create schema");
        let c1 = schema
            .find_conversation("test")
            .await
            .expect("failed to find conversation");
        let c2 = schema
            .find_conversation("test")
            .await
            .expect("failed to find conversation");
        assert_eq!(c1, c2);
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

    // #[tokio::test]
    // async fn test_history() {
    //     let schema = Schema::new(None).await.expect("failed to create schema");
    //     let conversation = schema
    //         .find_conversation("test")
    //         .await
    //         .expect("failed to define conversation");
    //     let role = Role::System;
    //     let message = "test message";
    //     schema
    //         .add_message(conversation, role, message)
    //         .await
    //         .expect("failed to add message");
    //     let messages = schema
    //         .history(conversation)
    //         .await
    //         .expect("failed to get history");
    //     assert_eq!(messages.len(), 1);
    //     assert_eq!(messages[0].role, role);
    //     assert_eq!(messages[0].content, message);
    // }
}
