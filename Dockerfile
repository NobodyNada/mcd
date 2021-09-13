FROM rust:alpine as builder
WORKDIR /usr/src/mcd
RUN apk add --no-cache musl-dev openssl-dev
ENV RUSTFLAGS=-Ctarget-feature=-crt-static
COPY . .
RUN cargo install --path .

FROM alpine:latest
WORKDIR /minecraft
EXPOSE 25565
RUN apk add openssl libgcc --no-cache
RUN apk add openjdk16-jre-headless --no-cache --repository=http://dl-cdn.alpinelinux.org/alpine/edge/testing
COPY --from=builder /usr/local/cargo/bin/mcd /usr/local/bin/mcd
CMD ["mcd"]
