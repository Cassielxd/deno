use serde::{Deserialize, Serialize};


#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub enum  IpcMessage{
    SentToWindow(SentToWindowMessage),
    SentToDeno(SentToDenoMessage),
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct SentToWindowMessage {
    pub id: String,//window 对应的label
    pub event: String,//对应的事件
    pub content: serde_json::Value,
}


#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct SentToDenoMessage {
    pub id: String,//deno 对应的id
    pub event: String,//对应的事件
    pub content: serde_json::Value,
}