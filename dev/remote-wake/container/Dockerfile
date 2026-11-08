# The bench's remote host: an sshd whose login sessions run the jobs the trial servers wait on.
#
# The image carries the tools the bench drives from outside: sshd for the servers under test, tc for the
# round-trip delay, iptables for the two fault cases, and the stub sacct the trigger code queries. The
# bench creates its users and writes their keys after the container starts, so no credential is in the
# image.
FROM ubuntu:24.04

RUN apt-get update \
  && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    openssh-server iproute2 iptables procps util-linux coreutils ca-certificates \
  && rm -rf /var/lib/apt/lists/* \
  && mkdir -p /run/sshd /run/bench /var/log \
  && chmod 1777 /run/bench

COPY sshd_config /etc/ssh/sshd_config
COPY entrypoint.sh /usr/local/bin/bench-entrypoint
COPY sacct /usr/local/bin/sacct
COPY bench-job /usr/local/bin/bench-job
RUN chmod 755 /usr/local/bin/bench-entrypoint /usr/local/bin/sacct /usr/local/bin/bench-job

EXPOSE 22
ENTRYPOINT ["/usr/local/bin/bench-entrypoint"]
