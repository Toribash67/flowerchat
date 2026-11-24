# The signaling server, and nothing else of the workspace: the app and the
# command-line client need GStreamer and a graphics card, and run on the
# machines of the people in the call.

FROM rust:1-alpine AS build

RUN apk add --no-cache musl-dev

WORKDIR /src

# Cargo wants every member of the workspace to be there, but builds only
# what the server depends on.
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

RUN cargo build --release --locked -p flowerchat-server

FROM alpine:3

# Inside the container the server listens on every interface; what reaches
# it from outside is up to the port mapping and the reverse proxy.
ENV FLOWERCHAT_LISTEN=0.0.0.0:80

COPY --from=build /src/target/release/flowerchat-server /usr/local/bin/flowerchat-server

EXPOSE 80

CMD ["flowerchat-server"]
