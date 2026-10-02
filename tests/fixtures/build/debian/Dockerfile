FROM mirror.gcr.io/library/debian:bookworm-slim
RUN groupadd -g 2000 grp && useradd -u 1500 -G grp app && rm /etc/debian_version
USER app
WORKDIR /home/app
RUN id > /tmp/id && pwd > /tmp/pwd && echo "$HOME" > /tmp/home
