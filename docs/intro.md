# What is taipei

Taipei is a Rust library that works with the tower crate to provide a set of building blocks for building reliable and optimal servers.

Here is a full example of a reliable server:
```rust
use axum::{error_handling::HandleErrorLayer, routing::get, Router};
use http::StatusCode;
use taipei::backpressure::InstrumentedRuntime as _;
use taipei::queue::{QueueError, DEFAULT_TIMEOUT};
use taipei::tokio::InstrumentedTokioRuntime;
use tower::{make::Shared, ServiceBuilder};

fn main() -> anyhow::Result<()> {
    // A tokio runtime that reports how busy the cores are, so taipei can tell
    // when every core is occupied.
    let instrumented = InstrumentedTokioRuntime::new()?;
    let instr = instrumented.instrumentation();
    let handle = instrumented.runtime.handle().clone();

    // Your application: an ordinary tower/axum service.
    let app = Router::new().route("/", get(|| async { "hello" }));

    // Wrap it in taipei's protection: a bounded queue in front of CPU
    // backpressure. Requests wait for a free core and are shed after the
    // deadline so the client can retry elsewhere. `worker` drives the queue
    // and must be spawned.
    let (service, worker) =
        taipei::compose::protect_queue(app, &instr, handle.clone(), DEFAULT_TIMEOUT);

    // A shed request surfaces as `QueueError`; turn it into 503 Service
    // Unavailable so the client knows to back off and retry.
    let service = Shared::new(
        ServiceBuilder::new()
            .layer(HandleErrorLayer::new(|e: QueueError| async move {
                (StatusCode::SERVICE_UNAVAILABLE, e.to_string())
            }))
            .service(service),
    );

    instrumented.runtime.block_on(async move {
        handle.spawn(worker.serve());
        let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await?;
        axum::serve(listener, service).await?;
        Ok(())
    })
}
```
And a full visualization

```sim
{ "sim": "queue-viz", "width": 960, "height": 520, "stage": "queue" }
```

We'll walk through why each component exists step-by-step.