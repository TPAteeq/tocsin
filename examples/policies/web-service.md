Page when:
- Checkout, payment, or login requests fail for many users at once.
- A database, cache, or queue is unreachable or out of connections.
- A service crashes, restarts in a loop, or runs out of memory.
- TLS certificates expire or every handshake to a dependency fails.

Do not page when:
- One user's request fails validation or returns a 4xx.
- A retry succeeds or a circuit breaker recovers on its own.
- Health checks, deploy notices, cron output, and cache misses.
