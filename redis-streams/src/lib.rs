#[macro_use]
extern crate wascc_codec as codec;

#[macro_use]
extern crate log;

use ::redis_streams::{
    Client, Connection, ErrorKind, RedisError, RedisResult, StreamCommands, Value,
};

use codec::capabilities::{
    CapabilityDescriptor, CapabilityProvider, Dispatcher, NullDispatcher, OperationDirection,
    OP_GET_CAPABILITY_DESCRIPTOR,
};
use codec::core::{OP_BIND_ACTOR, OP_REMOVE_ACTOR};
use codec::eventstreams::{self, Event, StreamQuery, StreamResults, WriteResponse};
use wascc_codec::core::CapabilityConfiguration;
use wascc_codec::{deserialize, serialize};

use std::error::Error;
use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

#[cfg(not(feature = "static_plugin"))]
capability_provider!(RedisStreamsProvider, RedisStreamsProvider::new);

const CAPABILITY_ID: &str = "wascc:eventstreams";
const SYSTEM_ACTOR: &str = "system";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const REVISION: u32 = 2; // Increment for each crates publish

pub struct RedisStreamsProvider {
    dispatcher: RwLock<Box<dyn Dispatcher>>,
    clients: Arc<RwLock<HashMap<String, Client>>>,
}

impl Default for RedisStreamsProvider {
    fn default() -> Self {
        match env_logger::try_init() {
            Ok(_) => {}
            Err(_) => {}
        };

        RedisStreamsProvider {
            dispatcher: RwLock::new(Box::new(NullDispatcher::new())),
            clients: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl RedisStreamsProvider {
    pub fn new() -> Self {
        Self::default()
    }

    fn configure(&self, config: CapabilityConfiguration) -> Result<Vec<u8>, Box<dyn Error>> {
        let c = initialize_client(config.clone())?;

        self.clients.write().unwrap().insert(config.module, c);
        Ok(vec![])
    }

    fn deconfigure(&self, actor: &str) -> Result<Vec<u8>, Box<dyn Error>> {
        self.clients.write().unwrap().remove(actor);
        Ok(vec![])
    }

    fn actor_con(&self, actor: &str) -> RedisResult<Connection> {
        let lock = self.clients.read().unwrap();
        if let Some(client) = lock.get(actor) {
            client.get_connection()
        } else {
            Err(RedisError::from((
                ErrorKind::InvalidClientConfig,
                "No client for this actor. Did the host configure it?",
            )))
        }
    }

    fn write_event(&self, actor: &str, event: Event) -> Result<Vec<u8>, Box<dyn Error>> {
        let data = map_to_tuples(event.values);
        let res: String = self.actor_con(actor)?.xadd(event.stream, "*", &data)?;
        Ok(serialize(WriteResponse { event_id: res })?)
    }

    fn query_stream(&self, actor: &str, query: StreamQuery) -> Result<Vec<u8>, Box<dyn Error>> {
        let sid = query.stream_id.to_string();
        let items = if let Some(time_range) = query.range {
            if query.count > 0 {
                self.actor_con(actor)?.xrange_count(
                    query.stream_id,
                    time_range.min_time,
                    time_range.max_time,
                    query.count,
                )?
            } else {
                self.actor_con(actor)?.xrange(
                    query.stream_id,
                    time_range.min_time,
                    time_range.max_time,
                )?
            }
        } else {
            if query.count > 0 {
                self.actor_con(actor)?
                    .xrange_count(query.stream_id, "-", "+", query.count)?
            } else {
                self.actor_con(actor)?.xrange(query.stream_id, "-", "+")?
            }
        };
        let mut events = Vec::new();

        for stream_id in items.ids {
            let newmap = stream_id
                .map
                .iter()
                .map(|(k, v)| (k.to_string(), val_to_string(v)))
                .collect::<HashMap<String, String>>();
            events.push(Event {
                event_id: stream_id.id,
                stream: sid.to_string(),
                values: newmap,
            });
        }

        Ok(serialize(StreamResults { events })?)
    }

    fn get_descriptor(&self) -> Result<Vec<u8>, Box<dyn Error>> {
        Ok(serialize(
            CapabilityDescriptor::builder()
            .id(CAPABILITY_ID)
            .name("waSCC Default Event Streams Provider (Redis)")
            .long_description("A capability provider exposing a streaming-read and append-only event streams interface")
            .version(VERSION)
            .revision(REVISION)
            .with_operation(eventstreams::OP_WRITE_EVENT, OperationDirection::ToProvider, "Writes an event to the end of a stream")
            .with_operation(eventstreams::OP_QUERY_STREAM, OperationDirection::ToProvider, "Queries a set of events from a stream")
            .build()
        )?)
    }
}

impl CapabilityProvider for RedisStreamsProvider {
    // Invoked by the runtime host to give this provider plugin the ability to communicate
    // with actors
    fn configure_dispatch(&self, dispatcher: Box<dyn Dispatcher>) -> Result<(), Box<dyn Error>> {
        trace!("Dispatcher received.");
        let mut lock = self.dispatcher.write().unwrap();
        *lock = dispatcher;

        Ok(())
    }

    // Invoked by host runtime to allow an actor to make use of the capability
    // All providers MUST handle the "configure" message, even if no work will be done
    fn handle_call(&self, actor: &str, op: &str, msg: &[u8]) -> Result<Vec<u8>, Box<dyn Error>> {
        trace!("Received host call from {}, operation - {}", actor, op);

        match op {
            OP_BIND_ACTOR if actor == SYSTEM_ACTOR => self.configure(deserialize(msg)?),
            OP_REMOVE_ACTOR if actor == SYSTEM_ACTOR => self.deconfigure(actor),
            OP_GET_CAPABILITY_DESCRIPTOR if actor == SYSTEM_ACTOR => self.get_descriptor(),
            eventstreams::OP_WRITE_EVENT => self.write_event(actor, deserialize(msg)?),
            eventstreams::OP_QUERY_STREAM => self.query_stream(actor, deserialize(msg)?),
            _ => Err("bad dispatch".into()),
        }
    }
}

const ENV_REDIS_URL: &str = "URL";

fn initialize_client(config: CapabilityConfiguration) -> Result<Client, Box<dyn Error>> {
    let redis_url = match config.values.get(ENV_REDIS_URL) {
        Some(v) => v,
        None => "redis://0.0.0.0:6379/",
    }
    .to_string();

    info!(
        "Attempting to connect {} to Redis at {}",
        config.module, redis_url
    );
    match Client::open(redis_url.as_ref()) {
        Ok(c) => Ok(c),
        Err(e) => Err(format!("Failed to connect to redis: {}", e).into()),
    }
}

fn map_to_tuples(map: HashMap<String, String>) -> Vec<(String, String)> {
    map.into_iter().collect()
}

// Extracts Redis arbitrary binary data as a string
fn val_to_string(val: &Value) -> String {
    if let Value::Data(vec) = val {
        ::std::str::from_utf8(&vec).unwrap().to_string()
    } else {
        "??".to_string()
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use redis_streams::Commands;
    use std::collections::HashMap;
    // **==- REQUIRES A RUNNING REDIS INSTANCE ON LOCALHOST -==**

    #[test]
    fn round_trip() {
        let prov = RedisStreamsProvider::new();
        let config = CapabilityConfiguration {
            module: "testing-actor".to_string(),
            values: gen_config(),
        };

        let c = initialize_client(config.clone()).unwrap();
        let _res: bool = c.get_connection().unwrap().del("my-stream").unwrap(); // make sure we start with an empty stream
        prov.configure(config).unwrap();

        for _ in 0..6 {
            let ev = Event {
                event_id: "".to_string(),
                stream: "my-stream".to_string(),
                values: gen_values(),
            };
            let buf = serialize(&ev).unwrap();
            let _res = prov
                .handle_call("testing-actor", eventstreams::OP_WRITE_EVENT, &buf)
                .unwrap();
        }

        let query = StreamQuery {
            count: 0,
            range: None,
            stream_id: "my-stream".to_string(),
        };
        let buf = serialize(&query).unwrap();
        let res = prov
            .handle_call("testing-actor", eventstreams::OP_QUERY_STREAM, &buf)
            .unwrap();
        let query_res = deserialize::<StreamResults>(res.as_ref()).unwrap();
        assert_eq!(6, query_res.events.len());
        assert_eq!(query_res.events[0].values["scruffy-looking"], "nerf-herder");
        let _res: bool = c.get_connection().unwrap().del("my-stream").unwrap(); // make sure we start with an empty stream
    }

    fn gen_config() -> HashMap<String, String> {
        let mut h = HashMap::new();
        h.insert("URL".to_string(), "redis://0.0.0.0:6379/".to_string());
        h
    }

    fn gen_values() -> HashMap<String, String> {
        let mut h = HashMap::new();
        h.insert("test".to_string(), "ok".to_string());
        h.insert("scruffy-looking".to_string(), "nerf-herder".to_string());
        h
    }
}
