Rust-Lightning is broken into a number of high-level structures with APIs to hook them
together, as well as APIs for you, the user, to provide external data.

The two most important structures which nearly every application of Rust-Lightning will
need to use are `ChannelManager` and `ChannelMonitor`. `ChannelManager` holds multiple
channels, routes payments between them and exposes a simple API to make and receive
payments. Individual `ChannelMonitor`s monitor the on-chain state of a channel, punish
counterparties if they misbehave, and force-close channels if they contain unresolved
HTLCs which are near expiration. The `ManyChannelMonitor` API provides a way for you to
receive `ChannelMonitorUpdate`s from `ChannelManager` and persist them to disk before the
channel steps forward.

There are several additional high-level structures that you may use either on the same
device as the `ChannelManager` or on a separate one. They include `Router`, which handles
receiving channel and node announcements, and calculates routes for sending payments.
`PeerManager` handles the authenticated and encrypted communication protocol, monitoring
for liveness of peers via `timer_tick_occured()` and routes messages to/from a
`ChannelManager` and `Router` instance as appropriate.

The ways each of the high-level structs communicate is public, so that you can easily add
hooks in between for special handling. Further, APIs for key generation, transaction
broadcasting, block fetching, and fee estimation exist must be implemented and the data
provided by you, the user.

At a high level, some of the common interfaces fit together as follows:


```

                     -----------------
                     | KeysInterface |  --------------
                     -----------------  | UserConfig |
         --------------------       |   --------------
  /------| MessageSendEvent |       |   |     ----------------
 |       --------------------       |   |     | FeeEstimator |
 |   (as MessageSendEventsProvider) |   |     ----------------
 |                         ^        |   |    /          |      ------------------------
 |                          \       |   |   /      ---------> | BroadcasterInterface |
 |                           \      |   |  /      /     |  \  ------------------------
 |                            \     v   v v      /      v   \
 |    (as                      ------------------       ----------------------
 |    ChannelMessageHandler)-> | ChannelManager | ----> | ManyChannelMonitor |
 v               /             ------------------       ----------------------
--------------- /                ^         (as EventsProvider)   ^
| PeerManager |-                 |              \     /         /
---------------                  |        -------\---/----------
 |              -----------------------  /        \ /
 |              | ChainWatchInterface | -          v
 |              -----------------------        ---------
 |                            |                | Event |
(as RoutingMessageHandler)    v                ---------
  \                   ----------
   -----------------> | Router |
                      ----------
```
