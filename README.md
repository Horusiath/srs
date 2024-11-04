# Scalable Redis Streams

A library to provide a way of reading Redis streams in a way to scales up to thousands concurrent stream readers.

## Example

```rust
#[tokio::main]
async fn main() -> Result<(), Error> {
    // create router
    let client = Client::open("redis://127.0.0.1/")?;
    let router = StreamRouter::with_options(&client, StreamRouterOptions {
        // number of reader worker threads - each worker gets its own
        // Redis connection which gets blocked while XREAD is pending
        worker_count: 8,
        // number of streams per worker - total number of Redis streams
        // that can be awaited on concurrently is `worker_count * xread_streams`
        xread_streams: 100,
        // how long does worker awaits before returning from XREAD
        xread_block_millis: Some(0),
        // how many messages should be returned in a single XREAD batch
        // this value ideally should be greater than `xread_streams`
        xread_count: Some(2 * 100),
    })?;

    // if you want to continue from specific point in Redis stream, specify
    // message id to continue reading from
    let last_message_id = Some("0-0".to_string());
    let mut reader = router.observe("test:stream".into(), last_message_id);

    while let Some((message_id, data)) = reader.recv().await {
        let payload: redis::Value = data["field"];
    }
}
```