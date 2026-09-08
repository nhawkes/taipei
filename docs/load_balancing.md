If there are multiple servers that can handle a request, the client has to decide which server to send a request to. In our model picking incorrectly will lead to the request being delayed by up to 100ms (in the queue) or being sent back for us to try a different server. Our information is always stale since the server is on a different machine.

If you do not control clients then the best you can do is picking randomly. And you cannot tell clients to retry. Let's start with the case where we own the clients and can make smarter decisions on them. Let's start by abstracting our server model so that we can show multiple servers.

```sim
{ "sim": "fleet", "width": 820, "height": 400 }
```


We can now model a client sending requests to a stateless service

```sim
{ "sim": "fan", "width": 820, "height": 520 }
```

Some constraints to be aware of. Each client picks the server and then sends all 10 of its requests to the same server. This is because handshakes are expensive. The first call to a new server does not know any TCP information such as link speed, and often needs to exchange information such as encryption keys. An interesting consequence of the fact that handshaking is proportional to round-trip-time is that if many machines need to talk to a machine far away, it's much faster to instead talk to a local machine that has a warm connection to that remote machine.

To talk about load balancing effectively we need to be able to model client experience. We usually model as percentiles. The slowest client experience is p100, the fastest p0, the average client is p50 and so on. Note p50 is the speed of the most average client (median). Not the average speed (mean). 

Let's simulate for an IO bound service, where one server can therefore serve 10 requests concurrently at max capacity.

```sim
{ "sim": "percentile", "width": 820, "height": 300 }
```

We count queue time as the time the request first enters a queue to when it successfully leaves a queue. This means it counts time in transit when retrying and can exceed 100ms. It's sensible to consider retries as a kind of global client-side queue.

We're modelling demand here as a single spike. There's no possible load balancing to be done on the first send since there's no communication between clients. If clients coordinated or asked the server then that would add latency to the request, which is probably better spent just trying and waiting for a queue timeout to come back if we're wrong.

If we have some continuous level of requests, less than global capacity but more than a single server's capacity then we start to get a feedback loop and this is where load-balancing can kick-in since it gets multiple rounds of requests and information to go off. In this simulation each client meets all servers quickly and so handshake time is zero after the first second.

```sim
{ "sim": "flow", "width": 820, "height": 520 }
```

The only information we get back from servers at this point is queue timeouts. Two sensible strategies here are always-random and repick-on-queue-timeout. It's better, however, if we can get feedback before a queue timeout. Let's define a load metric which is the number of requests in the queue, tie-broken by the number being currently processed. The strategy here is to pick two servers and then pick the one with the smallest load counter. This is called power of two.

```sim
{ "sim": "policies", "width": 820, "height": 760 }
```

The above sim gets loads counters by cheating and reading servers with no delay. In reality we can only get load counters after sending a request. The real scheme therefore is to pick randomly unless we have fresh load counters for both, in which case we pick the lowest. To smoothly interpolate between the two we'll pick random with a probability of `sqrt(staleness_in_ms)`%. 0 staleness picks best every time, 100ms staleness 10% of the time, 1000ms staleness always picks random. This is generally the best strategy and is called power-of-two. As we send a request we know that load will increase by one. So we can optimistically update the local counter. To keep load counters fresher each server can pick from a fixed subset.

To ensure we have fresh counters it's best to pick from a smaller pool, that way we keep counters for our pool warm with fewer requests. Exact selection size depends on client to server ratio. If we only have a single client then it's going to need to have a pool the size of the whole server set if we want to utilise all of them. Normally 5-10 is enough.

```sim
{ "sim": "pool", "width": 820, "height": 800 }
```

This simulation only has 20 clients. In order to have fresh load balancing data a client must have made a previous request recently. Many clients at a lower qps means they do not make requests often enough to obtain this fresh data.

A load balancer, meaning a separate server that just forwards on requests, therefore holds a few responsibilities. It firstly decreases the number of clients the server has, since 100 real clients going through 20 load balancers acts as 20 clients with fresh load balancing data. It means our real clients can be dumb and route randomly since they are often outside our control. And the downside of random-routing depends on our client-to-server ratio. Since load-balancing is cheap per client, random routing works well for it since there are many more clients than servers. The load balancer can also be close to the client reducing the roundtrip time needed to do a handshake if many of the clients are different such as for a web server.

```sim
{ "sim": "lb", "width": 820, "height": 1100 }
```

