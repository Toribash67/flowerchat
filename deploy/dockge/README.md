# Dockge deployment of the signaling server

This stack follows the Sengoku Jidai deployment:

- Image: `ghcr.io/toribash67/flowerchat-server:latest`
- Host port: `18084`
- Container port: `80`
- Nothing to keep between restarts: the server holds who is in which room
  in memory, and nothing else.
- Watchtower updates: enabled by label

Only the signaling server runs here. Voice, screens and chat go straight
between the people in a call and never pass through it.

## First setup

1. Create a new Dockge stack and paste `compose.yml`.
2. If the GHCR package is private, which it is by default for a private
   repository, either make the package public or log in to GHCR on the
   TrueNAS host and give Watchtower the same login.
3. Start the stack. `http://<truenas-host>:18084/healthz` answers `ok`, and
   `http://<truenas-host>:18084/` shows the web page.
4. Point a subdomain at it in nginx, as below.

## Downloads

The web page offers the installers attached to the repository's latest
release, which needs nothing set up while the repository is public.
`https://chat.flowerboi.se/api/release` lists what the page offers, and
`https://chat.flowerboi.se/download/windows` starts the download of the
latest Windows installer.

Should the repository become private, the server needs a token to see its
releases: a fine-grained token (Settings → Developer settings), for only
this repository, with read-only access to Contents and nothing else, set as
`FLOWERCHAT_GITHUB_TOKEN` under `environment` in `compose.yml`. Such tokens
expire, a year at most. Without a working token, the page says that no
installer has been published yet, and the server's log says why.

The server reaches out to `api.github.com` for this, at most every five
minutes. Nothing else needs to reach it.

## nginx

Unlike Sengoku Jidai, which polls over plain HTTP, flowerchat keeps a
WebSocket open for as long as someone is in a room. The proxy has to pass
the upgrade on, or joining fails:

```nginx
server {
    listen 443 ssl;
    server_name chat.flowerboi.se;   # whichever subdomain is chosen

    # ssl_certificate and ssl_certificate_key as for the other sites

    location / {
        proxy_pass http://127.0.0.1:18084;   # or the TrueNAS host's address
        proxy_http_version 1.1;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection "upgrade";
        proxy_set_header Host $host;
    }
}
```

The server pings every client every 20 seconds, which keeps the connection
inside nginx's default 60 second idle limit; no timeout needs raising.

Clients then join with the server address `wss://chat.flowerboi.se/ws`.

## Reaching each other

The server tells clients which STUN and TURN servers to use
(`FLOWERCHAT_ICE_SERVERS`). Without the variable they get a public STUN
server, with which most home connections reach each other directly. Two
people who cannot, because of carrier-grade NAT or a strict router, stay on
"Connecting…" until there is a TURN relay; none is set up yet. A relay needs
UDP ports forwarded to it and cannot go through nginx.

## CI/CD flow

Pushes to `main` run the "Server image" workflow: a format check, the
server's and the protocol's tests, and the image build. Only when those pass
is `ghcr.io/toribash67/flowerchat-server:latest` published, with the commit
SHA as a second tag. Pull requests build the image without publishing it.

GitHub Actions never contacts the TrueNAS host. Watchtower sees the new
`latest` image, pulls it and restarts the container. Everyone in a room is
dropped from it when that happens and has to join again, so changes to the
server are best pushed when nobody is in a call.
