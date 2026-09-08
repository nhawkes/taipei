# Other workloads

So far we've focused on `io_isolated`. Let's cover some other workloads:

## `io_nonisolated`
In this model each request can potentially take up the whole machine. This is common in languages where all parallel tasks happen through a spawn primitive which can run on multiple threads. It's best to avoid this and isolate all work for a request onto one cpu at a time. In particular this means avoiding tokio::spawn within requests, and only using it to spawn a task per request. To parallelize within a request use `futures::join` or run another executor like async_task inside a tokio task. This way one request can take up a max of one cpu instantaneously instead of starving other requests. The common reason for needing io_nonisolated is for a burst of io activity like fanning-out to many downstream services with some small cpu work like serialization.

## `cpu_isolated`
Here each request needs to do some expensive cpu calculation. Your server can handle at most n_cpus requests at a time since no multiplexing happens. Async is very optional here since there's only a few requests, thread per task is completely reasonable overhead. Since requests must take a long time (<100ms would not be counted as `cpu`) it may be worth having the load balancer do scheduling, since network time is much smaller than work time

## `cpu_nonisolated`
It may be tempting to parallelise expensive cpu work on a service to make it `x num_cpus` faster. Note this then again changes the effective capacity of your server down from `num_cpus` to `1`. Queue admission at least is easy

## `stateful`
It's common to have some requests need to be served by a particular machine. Databases are a good example. This means load-balancing can't function since the global pool of servers that can service the request is one. You should try to make stateful work as cheap as possible. Websockets and other long running connections prevent loadbalancing after the initial open. The solution is generally to have one machine hold the socket open, and then forward on to other workers the real work. One `stateful` server and one `io_isolated` server is generally easier to manage than a `stateful_io_isolated` server. Any sort of websocket + serverless function offering is this model.

## `dependent`
Sometimes a service will depend on another service. If that service handles rate-limiting and everything properly you should just be able to forward tenants through and the load on that service has no effect on how many your service needs to hold. It can be trusted to auto-scale or rate-limit its way out of any persistent queue delay. Note a queue delay in a dependency becomes processing delay in our service. If this isn't the case we might need to handle not overloading the dependent service ourselves, in which case we have a dependent workflow. There isn't much point giving queue timeouts for delays in a dependent service since retrying on another machine will not help the situation. We can potentially do the retries ourselves on behalf of the client. The exception is when the dependent service is locked to our service in some way, such as each service spawning exactly one dependent service or us having one service for each region of the dependent service. In this case we should not load balance across the dependent service and instead backpropagate pressure in the dependent service into our queue, since retries will now help. 
