We've propagated backpressure so far. Internal overloads in the service turn into queue timeouts. Queueing and queue timeouts feed into load balancing to turn local overloads into global overloads. Propagation should be sub-second, if the service remains overloaded for 1-10 seconds then we truely have more requests than capacity, globally. 

If your service is hit with more requests than it can handle there's really only two options:
- Autoscale
- Fail requests

# Autoscale

Here we just pay more money if we're overloaded in order to not be overloaded. Good if the requests are all valuable. Mirroring the local loadbalacing is a reasonable way to do this. If there are good targets for fresh requests then all is good. If all servers are bad targets add more machines. If things are a little too good try removing a machine and see how things get on. 

This is what serverless offerring do. The most common metric to use outside of this framework is CPU-utilisation. From the OS is fine for this since its naturally delayed. For anything modelling the an `io_isolated` service incorrectly as `isolated` note that this means it will be very expensive if one of the dependencies hangs. For example if your database requests suddently start taking approximately infinite time, then the service will autoscale to infinite just to hold all the connections open waiting for a response, and your wallet will autoscale to 0.

The load balancing algorithm needs to be kept in sync with the autoscaler's regionality. If the autoscaler can only add or remove machines globally in random regions then the load balancer should route requests globally as well. If the autoscaler works per region then the local load balancing algorithm should by default keep things in the same region. You do not want a state where requests are unneccisarily slow since they are going cross-region, and the auto-scaler isn't seeing any problem. A CPU-util only metric will be blind to this case. You need to make suer either requests are only going within the same region and so the region is seeing inflated metrics for the autoscaler to respond to, or ideally request temporily go cross-region but the autoscaler can see this and respond by adding more machines in the source region.

```sim
{ "sim": "autoscale", "width": 980, "height": 700 }
```

# Fail requests

If you can't meet demand the only option is to fail requests. This doesn't neccessarily mean failing forever, if its reasonable you can reject and the client is more than wecolme to exponmentially backed off until it succeeds if waiting is a deriable option for it. 

Not all requests are equally valuable. For a shopping website checkout requests are probably more important than browsing requests. And logged-in users are probably more important than logged-out requests. There's also usually some concept of fairness. If one user or tenant is using all the capacity you probably only want to fail their requests. Note some designs try and introduce this fairness at a server level.

For this section we will be assuming elastic modelling of tenants, that is we don't wamt to reserve capacity for a request that might come in later. The clearest example is if we have one server and a really important client that must always land on a free machine, under static reservation assumptions we can never serve anything in cause our important client wants the machine. All other sections are also based on elastic modelling but rate limiting is an area less common. Servers will often rate limit clients whilst free-capacity remains. Even with some separate static limit (say 100 requests per client), you still need some elastic modelling as well unless the server cannot be overloaded by all clients firing at their max rate limit which is uncommon. It is very reasonable in some scenarios to rate-limit clients before all servers are overloaded, and this also makes rate-limiting more predictable rather than being dependent on the overall state of the service. Static rate limit is well understood and out-of-scope here.

To reiterate our goals here, we want to introduce fairness so that sustained overloads (from 1 second to 10 seconds) drop requests in a configurable way that matches whatever priorities we have internally. We never want to drop requests unless the service is overloaded.

If we do nothing then requests will retry on queue timeout until the client gives up or some retry limit is reached. This is random so requests will be dropped from clients propotional to the number of requests they make. If you assume each request is equally important this is correct. If you assume tenants are equally important this is incorrect. Clients with more requests should have a higher drop percentage under this regime.

The trick taipei uses is it attributes queuing to tenants. As we are running we monitor is the server is accepting new requests or not (the gate). For every nanosecond it is not we add that to a shared counter. We then use an algorithm to attribute the queuing that occured to individual requests. This is approximate (if 4 requests start and end at the same time we don't know if any was individually more responsible for queueing than any of the other so must attribute all the same), but we only attribute for queuing that occurs while the request is on the server, so assuming some randomness its fair enough.

```sim
{ "sim": "blame", "width": 980, "height": 900 }
```

Once we have blame we start dropping requests based on it. We need to go from our blame numbers to some drop percentage. Theres many way to do this, for this we'll assume blame is accumerlated into Redis or something similar, and some seperate service reads redis numbers each second, predicts what the demand will be for the next second based on history, and then set a drop percentage for each client which would bring the total under budget. 

@TODO prose introducing the sim: what the three columns are, and why the loop takes three seconds to react.

```sim
{ "sim": "rate-limit", "width": 980, "height": 760 }
```

@TODO prose on what the presets show — in particular that a quiet tenant is never refused, and that a tenant sending a few expensive requests is refused as readily as one sending many cheap ones.

Note this schema works even if the tenants are sending different weighted requests. If Alice sends cheap requests and Bob sends heavy requests, we blame each correctly and are able to enforce only against the heavy client.