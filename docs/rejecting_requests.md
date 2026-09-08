Traffic is spiky. If a server gets hit with 1 million requests, the first thing it needs to do for the global service to successfully handle these requests is for the individual server to reject them so they can be retried.

If you're choosing to autoscale rather than outright reject, then that still needs rejection. Autoscaling takes some time and so requests need to be rejected so they can be automatically retried once the new capacity is online.

It is not safe to reject and retry a request once execution has started. Firstly the execution might have side-effects (you don't want to retry your database delete after execution). Secondly any CPU time that happened before rejection is now wasted.

The worst type of reject is a tcp drop. In this scenario the client simply doesn't get any response back. It has no idea if the request executed or not, and can't safely retry.

A common but limited approach is to use a fixed concurrency limit for the amount of concurrent requests. You can then either reject or queue if the server has too many requests

```sim
{ "sim": "queue-viz", "width": 960, "height": 520, "stage": "reject",
  "tabs": [{ "display": "Reject", "stage": "reject" },
           { "display": "Queue",  "stage": "wait" }] }
```

Remember unlike other failures, early rejects are perfectly safe to retry. They cost very little since the request hasn't started. Clients are expected to respond by immediately retrying on another server after getting more up-to-date load balancing behaviour. They are not an overall failure.

The main limitation is that concurrency needs tuning and may change. An obvious case is to see what happens when the IO speed changes (think about an incident which slows down db response time).
Neither reject nor queue is ideal. We don't want to reject when the server is about to become ready and we don't want to queue when it would be faster for another server to handle the request

# The queue

The queue in taipei attempts to get a best of both worlds of reject and queue. The strategy is to queue for a fixed time and then reject, smoothing over small spikes, hopefully avoiding rejecting on a server which is about to become ready.

Reject and retry takes a relatively long time. If both machines are geographically near each other they take 1ms-10ms to tell the client to retry. If the client is on a different continent it can take 50ms-200ms. We want to avoid retrying too early and instead wait if it's a temporary spike in traffic.

The decision a request needs to make is whether it will complete faster waiting on this server or by going back to the client to retry. After all, there may be other servers free immediately. Since this duration needs to be checked by the server, it's easier if all requests carry the same queue timeout. If this request landed on this server then we'll assume routing was sensible, and this was the best option at the time. Routing state is global and necessarily always a bit stale. Therefore retrying too quickly will just end up putting the request on the same server. For taipei we hardcode the queue timeout to 100ms. 

```sim
{ "sim": "queue-viz", "width": 960, "height": 520, "stage": "queue" }
```

Worth noting that if your server is constantly overloaded a queue timeout of 100ms will add approximately 100ms of latency to every request (try it). This is broadly known as bufferbloat as the buffer (the queue) is just adding latency for no gain here. For this reason we do not want to get in this scenario forever. The queue is there to absorb spikiness in requests, not work with constant overload. If you are constantly overloaded you need either autoscaling (add more capacity) or rate-limiting (choose which requests to block).

# The gate

We need to know when the server is capable of taking on more requests. For `isolated` the answer is we can take on requests as long as we have less requests than cpus, since one request saturates a cpu. For `io_isolated` the answer is more complex. On one hand the CPU being idle means we could be taking on more requests. On the other hand if all CPUs are busy when an IO task comes back, for example a database response, then we delay the request by the amount of time it takes to resume the task on a free core.

It's fairly common to use a manually tuned concurrency limit. Run the server on some average workload and find out the number of requests at which the response duration starts to deteriorate. The downside is that it takes time to tune, it's very easy to forget to retune and it doesn't react to dynamic changes. If performance is optimised anywhere or regressed the magic number goes out of date. If the workload of the server changes then it goes out of date. 

Consider for example the database is running twice as slow (IO speed = 0.5x). Now your server will probably hit its concurrency limit way too early, and requests will start retrying on other servers when there is in fact a shared resource that is overloaded not your server - retrying on another server will not help and so rejecting for retry does not make sense. Remember queue rejections are for problems which routing to another server might help with.

It's also quite common to do OS level CPU usage gates. If CPU usage goes above some threshold then reject all new requests. The issue with that is that CPU usage as a percentage has to be over some time period. On linux it is typically an average over a few seconds and so naturally delayed. A single CPU can only ever be at 100% or 0%. The upside to this approach is it avoids any program level instrumentation and so is uninvasive.

The approach that taipei recommends for `io_isolated` workloads is runtime level CPU tracking. Tokio will tell you when it starts or stops a thread for load. This gives instantaneous availability. Refusing to accept new requests when more than 50% of CPUs are active is a good default. It ensures that there are free cores available to promptly process IO results, whilst allowing accepting requests whilst waiting on those IO results.

```sim
{ "sim": "queue-viz", "width": 960, "height": 520, "stage": "queue", "gates": ["concurrency-limit", "os-cpu", "runtime-cpu"], "try": "io-speed" }
```

# Serverless

Note that any server will implicitly have to pick the approach here. You can't get away from the tradeoffs here, only have someone else pick your numbers and tradeoffs for you. AWS Lambda is billed by duration. That means its treating your workload as `isolated` since you are billed not just CPU time but also for IO time. They will virtually multiplex the CPUs so the machine effectively has many slow CPUs. In contrast Cloudflare Workers are billed by CPU time. That means they are treating your workload as `io_isolated` and will be oversubscribing CPUs to some extent. 

Consider the extreme case where your function is running with a bunch of other functions on the same machine. And they are all sleeping until 12.00pm. If you need compute at exactly 12.00pm when your database connection has come back then you're going to be fighting with lots of other functions also wanting their IO callback to run. In contrast with Lambda functions that CPU is reserved so there aren't as many functions that can be sleeping on the same machine. Which should be more consistent and less affected by neighbours, but that reservation in workflows will probably cost more as a result.

# Upstream tasks

It's worth mentioning that when we have two tasks, one is an I/O result that has returned, and the other is accepting a new request, it's always better to work on existing tasks before pulling in new work. Any work that has already been accepted needs driving to completion, whereas new work can be rejected. Tasks from new requests are referred to as upstream. If you are only maximising throughput and do not care about latency you should do work from existing requests first, and then if there are no tasks from existing jobs pull in new work from outstanding requests. Tokio does not allow task priorities and so cannot do this but some other runtimes can. In most cases though you do not want this behaviour since it increases request latency under load and would be better off using 50% CPU usage filter which somewhat mimics this behaviour.