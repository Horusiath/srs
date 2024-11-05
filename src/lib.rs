use loole::{Receiver, Sender};
use redis::streams::{StreamReadOptions, StreamReadReply};
use redis::Client;
use redis::Commands;
use redis::Connection;
use redis::RedisError;
use redis::RedisResult;
use redis::Value;
use std::collections::HashMap;
use std::error::Error;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::SeqCst;
use std::sync::Arc;
use std::thread::{sleep, JoinHandle};
use std::time::Duration;
use tokio::sync::mpsc::UnboundedReceiver;

pub type StreamKey = String;
pub type MessageId = String;

pub type StreamReader = tokio::sync::mpsc::UnboundedReceiver<(MessageId, RedisMap)>;

pub struct StreamRouter {
    buf: Sender<StreamHandle>,
    workers: Vec<Worker>,
    alive: Arc<AtomicBool>,
}

impl StreamRouter {
    pub fn new(client: &Client) -> Result<Self, RedisError> {
        Self::with_options(client, Default::default())
    }

    pub fn with_options(client: &Client, options: StreamRouterOptions) -> Result<Self, RedisError> {
        let alive = Arc::new(AtomicBool::new(true));
        let (tx, rx) = loole::unbounded();
        let mut workers = Vec::with_capacity(options.worker_count);
        for _ in 0..options.worker_count {
            let conn = client.get_connection()?;
            let worker = Worker::new(conn, tx.clone(), rx.clone(), alive.clone(), &options);
            workers.push(worker);
        }
        Ok(Self {
            buf: tx,
            workers,
            alive,
        })
    }

    pub fn observe(&self, stream_key: StreamKey, last_id: Option<MessageId>) -> StreamReader {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let last_id = last_id.unwrap_or_else(|| "0-0".to_string());
        let h = StreamHandle::new(stream_key, last_id, tx);
        self.buf.send(h).unwrap();
        rx
    }
}

impl Drop for StreamRouter {
    fn drop(&mut self) {
        self.alive.store(false, SeqCst);
    }
}

#[derive(Debug, Clone)]
pub struct StreamRouterOptions {
    pub worker_count: usize,
    pub xread_streams: usize,
    pub xread_block_millis: Option<usize>,
    pub xread_count: Option<usize>,
}

impl Default for StreamRouterOptions {
    fn default() -> Self {
        StreamRouterOptions {
            worker_count: std::thread::available_parallelism().unwrap().get(),
            xread_streams: 100,
            xread_block_millis: Some(0),
            xread_count: None,
        }
    }
}

struct Worker {
    handle: JoinHandle<()>,
}

impl Worker {
    fn new(
        conn: Connection,
        tx: Sender<StreamHandle>,
        rx: Receiver<StreamHandle>,
        alive: Arc<AtomicBool>,
        options: &StreamRouterOptions,
    ) -> Self {
        let mut xread_options = StreamReadOptions::default();
        if let Some(block_millis) = options.xread_block_millis {
            xread_options = xread_options.block(block_millis);
        }
        if let Some(count) = options.xread_count {
            xread_options = xread_options.count(count);
        }
        let count = options.xread_streams;
        let handle = std::thread::spawn(move || {
            if let Err(err) = Self::process_streams(conn, tx, rx, alive, xread_options, count) {
                tracing::error!("worker failed: {}", err);
            }
        });
        Self { handle }
    }

    fn process_streams(
        mut conn: Connection,
        tx: Sender<StreamHandle>,
        rx: Receiver<StreamHandle>,
        alive: Arc<AtomicBool>,
        options: StreamReadOptions,
        count: usize,
    ) -> RedisResult<()> {
        let mut stream_keys = Vec::with_capacity(count);
        let mut message_ids = Vec::with_capacity(count);
        let mut senders = HashMap::with_capacity(count);
        while alive.load(SeqCst) {
            if !Self::read_buf(&rx, &mut stream_keys, &mut message_ids, &mut senders) {
                break; // rx channel has closed
            }
            if stream_keys.is_empty() {
                tracing::warn!("Bug: read empty buf");
                sleep(Duration::from_millis(100));
                continue;
            }

            let result: StreamReadReply =
                conn.xread_options(&stream_keys, &message_ids, &options)?;

            for stream in result.keys {
                let mut remove_sender = false;
                if let Some((sender, idx)) = senders.get(stream.key.as_str()) {
                    for id in stream.ids {
                        let message_id = id.id;
                        let value = id.map;
                        message_ids[*idx] = message_id.clone(); //TODO: optimize
                        if let Err(err) = sender.send((message_id, value)) {
                            tracing::warn!("failed to send: {}", err);
                            remove_sender = true;
                        }
                    }
                }

                if remove_sender {
                    senders.remove(stream.key.as_str());
                }
            }

            Self::schedule_back(&tx, &mut stream_keys, &mut message_ids, &mut senders);
        }
        Ok(())
    }

    fn schedule_back<'a>(
        tx: &Sender<StreamHandle>,
        keys: &mut Vec<StreamKey>,
        ids: &mut Vec<MessageId>,
        senders: &mut HashMap<&'a str, (StreamSender, usize)>,
    ) {
        let mut keys = keys.drain(..);
        let mut ids = ids.drain(..);
        while let Some(key) = keys.next() {
            if let Some(last_id) = ids.next() {
                if let Some((sender, _)) = senders.remove(key.as_str()) {
                    let h = StreamHandle::new(key, last_id, sender);
                    if let Err(err) = tx.send(h) {
                        tracing::warn!("failed to reschedule: {}", err);
                        break;
                    }
                }
            }
        }
        senders.clear();
    }

    fn read_buf(
        rx: &Receiver<StreamHandle>,
        stream_keys: &mut Vec<StreamKey>,
        message_ids: &mut Vec<MessageId>,
        senders: &mut HashMap<&'static str, (StreamSender, usize)>,
    ) -> bool {
        // try to receive first element - block thread if there's none
        let mut count = stream_keys.capacity();
        if let Ok(h) = rx.recv() {
            // senders and stream_keys have bound lifetimes and fixed internal buffers
            // since API users are using StreamKeys => String, we want to avoid allocations
            let key_ref: &'static str = unsafe { std::mem::transmute(h.key.as_str()) };
            senders.insert(key_ref, (h.sender, stream_keys.len()));
            stream_keys.push(h.key);
            message_ids.push(h.last_id);

            count -= 1;
            if count == 0 {
                return true;
            }

            // try to fill more without blocking if there's anything on the receiver
            while let Ok(h) = rx.try_recv() {
                let key_ref: &'static str = unsafe { std::mem::transmute(h.key.as_str()) };
                senders.insert(key_ref, (h.sender, stream_keys.len()));
                stream_keys.push(h.key);
                message_ids.push(h.last_id);

                count -= 1;
                if count == 0 {
                    return true;
                }
            }
            true
        } else {
            false
        }
    }
}

type RedisMap = HashMap<String, Value>;
type StreamSender = tokio::sync::mpsc::UnboundedSender<(MessageId, RedisMap)>;

struct StreamHandle {
    key: StreamKey,
    last_id: MessageId,
    sender: StreamSender,
}

impl StreamHandle {
    fn new(key: StreamKey, last_id: MessageId, sender: StreamSender) -> Self {
        StreamHandle {
            key,
            last_id,
            sender,
        }
    }
}

#[cfg(test)]
mod test {
    use crate::{MessageId, StreamRouter};
    use rand::random;
    use redis::{Client, Commands, FromRedisValue};
    use tokio::task::JoinSet;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn multi_worker_preexisting_messages() {
        const ROUTES_COUNT: usize = 200;
        const MSG_PER_ROUTE: usize = 10;
        let mut client = Client::open("redis://127.0.0.1/").unwrap();
        let mut keys = init_streams(&mut client, ROUTES_COUNT, MSG_PER_ROUTE);

        let router = StreamRouter::new(&client).unwrap();

        let mut join_set = JoinSet::new();
        for key in keys {
            let mut observer = router.observe(key.clone(), None);
            join_set.spawn(async move {
                for i in 0..MSG_PER_ROUTE {
                    let (_msg_id, map) = observer.recv().await.unwrap();
                    let value = String::from_redis_value(&map["data"]).unwrap();
                    assert_eq!(value, format!("{}-{}", key, i));
                }
            });
        }

        while let Some(t) = join_set.join_next().await {
            t.unwrap();
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn multi_worker_live_messages() {
        const ROUTES_COUNT: usize = 200;
        const MSG_PER_ROUTE: usize = 10;
        let mut client = Client::open("redis://127.0.0.1/").unwrap();
        let mut keys = init_streams(&mut client, ROUTES_COUNT, 0);

        let router = StreamRouter::new(&client).unwrap();

        let mut join_set = JoinSet::new();
        for key in keys.iter().cloned() {
            let mut observer = router.observe(key.clone(), None);
            join_set.spawn(async move {
                for i in 0..MSG_PER_ROUTE {
                    let (_msg_id, map) = observer.recv().await.unwrap();
                    let value = String::from_redis_value(&map["data"]).unwrap();
                    assert_eq!(value, format!("{}-{}", key, i));
                }
            });
        }

        for msg_idx in 0..MSG_PER_ROUTE {
            for key in keys.iter() {
                let data = format!("{}-{}", key, msg_idx);
                let _: MessageId = client.xadd(&key, "*", &[("data", data)]).unwrap();
            }
        }

        while let Some(t) = join_set.join_next().await {
            t.unwrap();
        }
    }

    #[tokio::test]
    async fn stream_reader_continue_from() {
        let mut client = Client::open("redis://127.0.0.1/").unwrap();
        let key = format!("test:{}:{}", random::<u32>(), 0);
        let _: MessageId = client.xadd(&key, "*", &[("data", 1)]).unwrap();
        let m2: MessageId = client.xadd(&key, "*", &[("data", 2)]).unwrap();
        let m3: MessageId = client.xadd(&key, "*", &[("data", 3)]).unwrap();

        let router = StreamRouter::new(&client).unwrap();
        let mut observer = router.observe(key, Some(m2));

        let (msg_id, m) = observer.recv().await.unwrap();
        assert_eq!(msg_id, m3);
        assert_eq!(u32::from_redis_value(&m["data"]).unwrap(), 3);
    }

    fn init_streams(
        client: &mut Client,
        stream_count: usize,
        msgs_per_stream: usize,
    ) -> Vec<String> {
        let test_prefix: u32 = random();
        let mut keys = Vec::with_capacity(stream_count);
        for worker_idx in 0..stream_count {
            let key = format!("test:{}:{}", test_prefix, worker_idx);
            for msg_idx in 0..msgs_per_stream {
                let data = format!("{}-{}", key, msg_idx);
                let _: MessageId = client.xadd(&key, "*", &[("data", data)]).unwrap();
            }
            keys.push(key);
        }
        keys
    }
}
