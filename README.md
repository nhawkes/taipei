# taipei

[Taipei](https://github.com/nhawkes/taipei) is a library that integrates with tower to enable writing servers that behave well under stress without tuning.

Here is a full example of a reliable server:
```rust,no_run
use axum::{error_handling::HandleErrorLayer, routing::get, Router};
use http::StatusCode;
use taipei::backpressure::{CpuBackpressureLayer, InstrumentedRuntime as _};
use taipei::queue::{QueueError, QueueLayer, DEFAULT_QUEUE_TIMEOUT};
use taipei::tokio::InstrumentedTokioRuntime;
use tower::{make::Shared, ServiceBuilder};

fn main() -> anyhow::Result<()> {
    // instrument tokio's CPU usage
    let instrumented = InstrumentedTokioRuntime::new()?;
    let instr = instrumented.instrumentation();
    let handle = instrumented.runtime.handle().clone();

    let my_service = Router::new().route("/", get(|| async { "hello" }));

    // hold requests while CPU usage is above 50%
    let inner = ServiceBuilder::new()
        .layer(CpuBackpressureLayer::new(&instr))
        .service(my_service);
    let (service, worker) = QueueLayer::new(DEFAULT_QUEUE_TIMEOUT).build(inner, handle.clone());

    // handle timeout in queue and other errors
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
[Read more](https://taipei-book.pages.dev/intro)
