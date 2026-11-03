FROM docker.io/library/ubuntu:20.04

# This file allows building a Generic debug container image
# based on one or multiple pre-built Linux binaries.
# Some defaults are set to polkadot but all can be overridden.

SHELL ["/bin/bash", "-c"]

# metadata
ARG VCS_REF
ARG BUILD_DATE
ARG IMAGE_NAME
ARG BINARY=polkadot

ARG DOC_URL=https://github.com/paritytech/polkadot-sdk
ARG DESCRIPTION="Polkadot: a platform for web3"
ARG AUTHORS="devops-team@parity.io"
ARG VENDOR="Parity Technologies"

LABEL io.parity.image.authors=${AUTHORS} \
	io.parity.image.vendor="${VENDOR}" \
	io.parity.image.revision="${VCS_REF}" \
	io.parity.image.title="${IMAGE_NAME}" \
	io.parity.image.created="${BUILD_DATE}" \
	io.parity.image.documentation="${DOC_URL}" \
	io.parity.image.description="${DESCRIPTION}" \
	io.parity.image.source="https://github.com/paritytech/polkadot-sdk/blob/${VCS_REF}/docker/dockerfiles/binary_injected_debug.Dockerfile"

# show backtraces
ENV RUST_BACKTRACE 1

# install tools and dependencies
RUN apt-get update && \
	DEBIAN_FRONTEND=noninteractive apt-get install -y \
	libssl1.1 \
	ca-certificates && \
	# apt cleanup
	apt-get autoremove -y && \
	apt-get clean && \
	find /var/lib/apt/lists/ -type f -not -name lock -delete; \
	# add user
	useradd -m -u 1000 -U -s /bin/sh -d /data polkadot && \
	mkdir -p /data && \
	chown -R polkadot:polkadot /data

# add binary to docker image
COPY ./artifacts/* /usr/local/bin/
RUN chmod -R a+rx "/usr/local/bin"

USER polkadot
ENV BINARY=${BINARY}

EXPOSE 30333 9933 9944 9615
VOLUME ["/data"]

ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
CMD ["--help"]
