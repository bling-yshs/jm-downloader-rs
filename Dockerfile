FROM alpine:3.23.2

RUN apk add --no-cache tzdata ca-certificates ghostscript

WORKDIR /app

COPY /target/x86_64-unknown-linux-musl/release/jm-downloader-rs .

COPY Rocket.toml log4rs.yaml ./

CMD ./jm-downloader-rs
