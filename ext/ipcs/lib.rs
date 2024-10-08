// Copyright 2018-2024 the Deno authors. All rights reserved. MIT license.


use deno_core::error::AnyError;

use deno_core::{op2};

use deno_core::OpState;

use events_manager::EventsManager;
use messages::{IpcMessage, SentToDenoMessage, SentToWindowMessage};
use serde::Deserialize;
use serde::Serialize;
use uuid::Uuid;
use std::cell::RefCell;

use std::rc::Rc;

use tokio::sync::mpsc::{self};



pub mod messages;
pub mod events_manager;



pub type IpcSender = async_channel::Sender<IpcMessage>;
pub type IpcReceiver = async_channel::Receiver<IpcMessage>;

deno_core::extension!(
  deno_ipc,
  deps = [deno_webidl,deno_web ],
  ops = [
    listen_event,
    send_to_host,
    send_to_deno
  ],
   esm_entry_point = "ext:deno_ipcs/01_ipcs.js",
    esm  = [ "01_ipcs.js"],
    options = {
        sender:IpcSender,
        events_manager:EventsManager
    },
    state = |state, options| {
        state.put(options.sender);
        state.put(options.events_manager);

  },
);
#[derive(Serialize, Deserialize, Debug)]
struct EventListen {
    name: String,
}

//事件监听
#[op2(async)]
#[serde]
async fn listen_event(
    state: Rc<RefCell<OpState>>,
    #[serde]  event: EventListen
) -> Result<serde_json::Value, AnyError> {
    let (listener, mut receiver) = mpsc::channel(1);
    let listener_id = Uuid::new_v4();

    let events_manager: EventsManager = {
        let state: std::cell::Ref<'_, OpState> = state.try_borrow()?;

        state.try_borrow::<EventsManager>().unwrap().clone()
    };
    events_manager
        .listen_on(event.name.clone(), listener_id, listener)
        .await;
    let event_response = receiver.recv().await;

    events_manager.unlisten_from(event.name, listener_id).await;
    Ok(event_response.unwrap())
}
//发送到主进程
#[op2(async)]
async fn send_to_host(
    state: Rc<RefCell<OpState>>,
    #[serde] args: SentToWindowMessage,
) -> Result<(), AnyError> {
    let sender: IpcSender = {
        let state = state.try_borrow()?;
        state
            .try_borrow::<IpcSender>()
            .unwrap()
            .clone()
    };

    sender
        .send(IpcMessage::SentToWindow(args))
        .await
        .unwrap();
    Ok(())
}
//deno 之间通信 发送到其他deno线程
#[op2(async)]
async fn send_to_deno(
    state: Rc<RefCell<OpState>>,
    #[serde] args: SentToDenoMessage,
) -> Result<(), AnyError> {
    let sender: IpcSender = {
        let state = state.try_borrow()?;
        state
            .try_borrow::<IpcSender>()
            .unwrap()
            .clone()
    };

    sender
        .send(IpcMessage::SentToDeno(args))
        .await
        .unwrap();
    Ok(())
}