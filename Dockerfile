FROM rust:1.67 as builder
WORKDIR /usr/src/hcloud-fip-controller
COPY . .
RUN cargo install --path .

FROM debian:bullseye-slim
COPY --from=builder /usr/local/cargo/bin/hcloud-fip-controller /usr/local/bin/hcloud-fip-controller
CMD ["hcloud-fip-controller"]
