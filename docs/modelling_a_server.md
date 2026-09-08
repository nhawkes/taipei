A typical server will accept tcp connections. The client will send requests over those connections. And the server will respond to these requests.

Lets take a sinple http server. We run GET `/ping` and it responds pong.

```rust
#[tokio::main]
async fn main() {
    let app = Router::new().route("/ping", get(|| async { "pong" }));
    let listener = TcpListener::bind("0.0.0.0:3000").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
```

Click the button below to send a request

```sim
{ "sim": "queue-viz", "width": 960, "height": 520, "stage": "app", "workload": "cpu", "charts": false, "manual": true }
```

As you can see, when a request gets sent it first sends a TCP syn to the server. This is handled by the kernel which will wake any program waiting on it. The worken program runs on the CPU calculates the response and returns a response the client. As with all models its a slight simplicifation but will serve us well.

We can also compare this to a two very bad servers:

```rust tab="accept then hang"
loop {
    let (conn, _) = listener.accept().await.unwrap();
    tokio::spawn(async move {
        // A bug spins the handler forever: the reply never comes, and the
        // worker the connection landed on is never freed.
        let _held = conn;
        loop { std::hint::spin_loop() }
    });
}
```

```rust tab="never accept"
let listener = TcpListener::bind("0.0.0.0:3000").await.unwrap();
// accept() is never called: handshakes complete in the kernel and sit in
// its accept queue until each client gives up.
std::future::pending::<()>().await
```

```sim
{ "sim": "queue-viz", "width": 960, "height": 520, "stage": "app", "workload": "cpu", "charts": false, "manual": true, "servers": ["good", "never-accept", "accept-hang"] }
```

Since this server just does light cpu work, and each request completes on a single thread we'll name this type of server `isolated`. This is the simplest type of server. For this type of server the bottleneck is how quickly we can accept, read and reply to the request. And we can handle many requests.

We can start modelling requests as coming in at some frequency with a bit of noise


```sim
{ "sim": "queue-viz", "width": 960, "height": 520, "stage": "app", "workload": "cpu", "try": "arrivals" }
```

One of the problems with servers is that if the rate of incoming requests gets too high (try it), our good server starts acting like our bad servers. Dropping requests by either not accepting or (worse) accepting and then never getting round to replying.

The reason this type of request is simple is that because there is no time the CPU is waiting on some IO resource. We have a fixed number of cpus we will spawn one thread per cpu (spawning more threads will be slower since the operating system then has to deal with shuffling our m threads onto n cores). And requests always take the same amount of time after we accept them.

Most webservers or microservices also do some form of IO. For example reading a file or connecting to a database. We can model requests as requiring alternating cpu_time (needs a cpu free) and io_time (must wait, infinitely parallisable). We'll call this type of server  `io_isolated`. Most servers look something like this:

```sim
{ "sim": "queue-viz", "width": 960, "height": 520, "stage": "app" }
```

Unlike with `isolated`, `io_isolated` can have different processing times. And it comes down to when io completes, how fast can we find a core to land on.

One way to make sure requests are successful is simply to have more servers than are needed to serve the total amount of requests. The simplest case of this is having one big server and not too many requests. The issues with this are:
- If you unexpectedly get more requests, *all* requests fail (not just the excess)
- If you unexpectedly get less requests, you're paying more than neccessary

We will be exploring how to do better than this.