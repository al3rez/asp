# Tailscale networking architecture

## Sources and revision

Read [How Tailscale works](https://tailscale.com/blog/how-tailscale-works), [How NAT traversal works](https://tailscale.com/blog/how-nat-traversal-works), and current [connection types](https://tailscale.com/docs/reference/connection-types). Inspected [tailscale/tailscale](https://github.com/tailscale/tailscale) revision `af4b7b03633a5fe06e6fc274e27fc369007a0e66`, focusing on `wgengine/magicsock`, `disco`, `net/netcheck`, `net/portmapper`, `derp`, and relay management.

## Separation of planes

Tailscale’s control plane distributes node identity, public keys, policy, relay map, and candidate endpoints. Its data plane uses end-to-end WireGuard. `magicsock` presents WireGuard with a stable packet-connection abstraction while dynamically choosing among actual paths.

Candidate endpoints include local IPv4/IPv6 addresses, STUN-observed public UDP mappings, port-mapper allocations (PCP/NAT-PMP/UPnP where allowed), configured endpoints, DERP, and newer peer relays. Authenticated discovery (“disco”) ping/pong packets probe candidates, punch stateful firewalls, measure latency, and refresh liveness. Candidate information travels through the coordination/DERP side channel. Path selection remembers a trusted best address but continuously re-evaluates reachability and latency.

All connections can start through DERP, which provides immediate HTTPS-reachable fallback and signaling, then upgrade to direct UDP. Current Tailscale also supports user-operated peer relays before falling back to DERP. Path switches are transparent to WireGuard/application traffic.

DERP relays encrypted packets based on peer identity and cannot decrypt WireGuard payloads. It is designed for connectivity and signaling, not high-throughput QoS; direct paths generally have lower latency/higher throughput.

## Why this is not a small ASP feature

Reliable peer connectivity requires more than a STUN request:

- enumerate interface and public candidates;
- share candidates through an authenticated coordination channel;
- send simultaneous probes from the same UDP socket as the data protocol;
- handle endpoint-dependent (“hard”) NATs and port prediction/port mapping;
- keep mappings alive;
- measure and select paths, including asymmetric cases;
- detect failure and fall back without losing identity;
- operate globally reachable relays and prevent abuse;
- preserve end-to-end security across those relays.

RoSE’s best-effort STUN/SSH signaling helps common stateful-firewall cases but is not equivalent to magicsock/DERP.

## ASP v0 decision

ASP v0 runs on a directly reachable UDP address or an existing Tailscale IP. Tailscale then supplies device identity/policy, NAT traversal, direct connections, roaming under the stable tailnet address, and relay fallback. ASP still benefits from QUIC for multiplexed reliable/unreliable channels and connection migration within that route.

This layering may add WireGuard plus QUIC encryption overhead, but it avoids years of security/connectivity work. Measure overhead before considering integration.

## Minimal long-term subset

If ASP eventually must work without Tailscale, the minimum credible subsystem is:

1. authenticated device registration and peer rendezvous;
2. candidate gathering (IPv6/LAN, STUN, optional port mapping);
3. same-socket authenticated probes and UDP hole punching;
4. RTT/liveness-based path choice with continuous re-probing;
5. a universally reachable encrypted relay fallback;
6. stable peer identity and session authorization across path changes;
7. telemetry and abuse/rate controls.

That is already Tailscale-like connectivity. The preferred v1 path is integration with Tailscale/Headscale or another mature overlay, not an ASP-specific NAT stack.

