FROM docker.io/parity/base-bin:latest

# This file builds the official eth-rpc release image by injecting the
# pre-built, signed `eth-rpc` release artifact into the base image.
# The runtime layout is kept in sync with the source-built image at
# substrate/frame/revive/rpc/dockerfiles/eth-rpc/Dockerfile.

# metadata
ARG VCS_REF
ARG BUILD_DATE
ARG IMAGE_NAME
# That can be a single one or a comma separated list
ARG BINARY=eth-rpc

LABEL io.parity.image.authors="devops-team@parity.io" \
	io.parity.image.vendor="Parity Technologies" \
	io.parity.image.title="${IMAGE_NAME}" \
	io.parity.image.description="Ethereum JSON-RPC proxy for pallet-revive. This is the official Parity image with an injected binary." \
	io.parity.image.source="https://github.com/paritytech/polkadot-sdk/blob/${VCS_REF}/docker/dockerfiles/eth-rpc/eth-rpc_injected.Dockerfile" \
	io.parity.image.revision="${VCS_REF}" \
	io.parity.image.created="${BUILD_DATE}" \
	io.parity.image.documentation="https://github.com/paritytech/polkadot-sdk/"

# show backtraces
ENV RUST_BACKTRACE 1

USER root

# add the pre-built eth-rpc binary to the docker image
COPY bin/* /usr/local/bin/

RUN chmod -R a+rx "/usr/local/bin" && \
	useradd -m -u 1001 -U -s /bin/sh -d /polkadot polkadot && \
	rm -rf /usr/bin /usr/sbin && \
	# check if executable works in this container
	/usr/local/bin/eth-rpc --version

USER polkadot

# 8545 is the default port for the RPC server
# 9616 is the default port for the prometheus metrics
EXPOSE 8545 9616

ENTRYPOINT ["/usr/local/bin/eth-rpc"]

# We call the help by default
CMD ["--help"]
