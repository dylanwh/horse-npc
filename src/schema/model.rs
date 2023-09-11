use async_openai::types::{ChatCompletionRequestMessage, ChatCompletionResponseMessage};
use eyre::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub enum Message {
    Content {
        role: Role,
        content: String,
    },
    Function {
        role: Role,
        fn_name: String,
        fn_args: String,
    },
}
impl Message {
    pub fn new<S>(role: Role, content: S) -> Self
    where
        S: AsRef<str>,
    {
        let content = content.as_ref().to_owned();
        Self::Content { role, content }
    }

    pub fn role(&self) -> Role {
        match self {
            Message::Content { role, .. } => *role,
            Message::Function { role, .. } => *role,
        }
    }
    pub fn content(&self) -> String {
        match self {
            Message::Content { content, .. } => content.to_owned(),
            Message::Function { fn_name, fn_args, .. } => format!("{fn_name}({fn_args})"),
        }
    }
}

#[derive(Debug, Ord, PartialOrd, Eq, PartialEq, Clone, Copy)]
pub struct Conversation(pub(super) i64);


#[derive(Debug, Ord, PartialOrd, Eq, PartialEq, Clone, Copy, Serialize, Deserialize)]
pub enum Role {
    System,
    User,
    Assistant,
    Function,
}

impl From<Role> for async_openai::types::Role {
    fn from(role: Role) -> Self {
        match role {
            Role::System => async_openai::types::Role::System,
            Role::User => async_openai::types::Role::User,
            Role::Assistant => async_openai::types::Role::Assistant,
            Role::Function => async_openai::types::Role::Function,
        }
    }
}

impl From<async_openai::types::Role> for Role {
    fn from(role: async_openai::types::Role) -> Self {
        match role {
            async_openai::types::Role::System => Role::System,
            async_openai::types::Role::User => Role::User,
            async_openai::types::Role::Assistant => Role::Assistant,
            async_openai::types::Role::Function => Role::Function,
        }
    }
}

impl TryFrom<ChatCompletionResponseMessage> for Message {
    type Error = eyre::Error;

    fn try_from(response: ChatCompletionResponseMessage) -> Result<Self> {
        let message = match (response.content, response.function_call) {
            (Some(s), None) => Message::Content {
                role: response.role.into(),
                content: s,
            },
            (None, Some(f)) => Message::Function {
                role: response.role.into(),
                fn_name: f.name,
                fn_args: f.arguments,
            },
            _ => unreachable!("Invalid response from OpenAI"),
        };

        Ok(message)
    }
}

impl TryFrom<&Message> for ChatCompletionRequestMessage {
    type Error = eyre::Error;

    fn try_from(message: &Message) -> Result<Self> {
        let (content, function_call) = match message {
            Message::Content { role: _, content } => (Some(content), None),
            Message::Function {
                role: _,
                fn_name,
                fn_args,
            } => (
                None,
                Some(async_openai::types::FunctionCall {
                    name: fn_name.to_owned(),
                    arguments: fn_args.to_owned(),
                }),
            ),
        };
        Ok(Self {
            name: function_call.clone().map(|f| f.name),
            role: message.role().into(),
            content: content.cloned(),
            function_call,
        })
    }
}
