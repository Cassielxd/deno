// Copyright 2018-2024 the Deno authors. All rights reserved. MIT license.
import { core, primordials } from "ext:core/mod.js";
import {
    defineEventHandler,
    EventTarget,
    setIsTrusted,
    setTarget,
  } from "ext:deno_web/02_event.js";
  import { defer } from "ext:deno_web/02_timers.js";
import { listen_event, send_to_deno, send_to_host } from "ext:core/ops";
const {
    ObjectPrototypeIsPrototypeOf,
    Symbol,
    SymbolFor
} = primordials;
import { createFilteredInspectProxy } from "ext:deno_console/01_console.js";
import * as webidl from "ext:deno_webidl/00_webidl.js";

const _name = Symbol("[[name]]");
const _closed = Symbol("[[closed]]");
async function sendToDeno(key, event, content) {
    await send_to_deno({ id: key, event, content });
}
async function sendToHost(id, event, content) {
    await send_to_host({ id, event, content });
}

let channelMap = new Map();
async function recv(key, ipc) {
    let channels = channelMap.get(key);
    while (channels.length > 0) {
        const message = await ipc.nextRequest();
        if (message === null) {
            break;
        }
        dispatch(null, key, message);
    }
}
function dispatch(source, name, data) {
    let channels = channelMap.get(name);
    for (let i = 0; i < channels.length; ++i) {
        const channel = channels[i];
        if (channel === source) continue; // Don't self-send.
        if (channel[_name] !== name) continue;
        const go = () => {
            const event = new MessageEvent("message", {
                data,
                origin: "http://127.0.0.1",
            });
            setIsTrusted(event, true);
            setTarget(event, channel);
            channel.dispatchEvent(event);
        };

        defer(go);
    }
}
class IpcBroadcastChannel extends EventTarget {

    [_name];
    [_closed] = false;
    constructor(name) {
        super();
        const prefix = "Failed to construct 'IpcBroadcastChannel'";
         webidl.requiredArguments(arguments.length, 1, prefix);
         this[_name] = webidl.converters["DOMString"](name, prefix, "Argument 1");
        if (channelMap.has(this[_name])) {
            channelMap.get(this[_name]).push(this);
        } else {
            channelMap.set(this[_name], [this]);
            recv(this[_name], this);
        }
    }
     get name() {
        return this[_name];
      }

    postToWindowMessage({key,name,message}) {
        defer(() => {
            send_to_host({
                id: key?key:"ALL",
                event: name?name:this.name,
                content: message,
            });
        });
    }
    postMessage({key,name,message}) {
        defer(() => {
            send_to_deno({
                id: key?key:"ALL",
                event: name?name:this.name,
                content: message,
            });
        });
    }
    close() {
        channelMap.get(this.name).splice(channelMap.get(this.name).indexOf(this), 1);
        if (channelMap.get(this.name).length === 0) {
            channelMap.delete(this.name);
        }
    }
    async nextRequest() {
        let nextRequest;
        try {
            nextRequest = await listen_event({ name: this.name });
        } catch (error) {
            console.log(error);
            throw error;
        }
        const request = nextRequest;
        return request;
    }
    [SymbolFor("Deno.privateCustomInspect")](inspect, inspectOptions) {
        return inspect(
          createFilteredInspectProxy({
            object: this,
            evaluate: ObjectPrototypeIsPrototypeOf(IpcBroadcastChannelPrototype, this),
            keys: [
              "name",
              "onmessage",
              "onmessageerror",
            ],
          }),
          inspectOptions,
        );
      }
}
defineEventHandler(IpcBroadcastChannel.prototype, "message");
defineEventHandler(IpcBroadcastChannel.prototype, "messageerror");
const IpcBroadcastChannelPrototype = IpcBroadcastChannel.prototype;
globalThis.IPcs = {
    sendToDeno,
    sendToHost,
    IpcBroadcastChannel
};
globalThis.IpcBroadcastChannel = IpcBroadcastChannel;
