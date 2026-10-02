FROM mirror.gcr.io/library/alpine:3.20
ENV GREETING="hello world" APP=/app
WORKDIR $APP
COPY hello.txt ./
COPY tree/ data/
COPY *.conf /etc/demo/
RUN rm /etc/motd && adduser -D -u 1234 builder && echo "$GREETING" > greeting && ln -s greeting link
RUN apk add --no-cache tini
USER builder
RUN id -u > /tmp/uid
LABEL org.example.demo="yes"
EXPOSE 8080 53/udp
ENTRYPOINT ["/bin/sh", "-c"]
CMD ["cat /app/greeting"]
