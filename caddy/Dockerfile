# The official binary carries cap_net_bind_service, which prevents exec when
# ALL capabilities are dropped. Our listener is 8443, so remove that file cap.
FROM caddy:2-alpine@sha256:eb4d35b7452d1ab6171fbdbe8d9275b2301dad6dad92115d91e19fb9b24d510f
RUN apk add --no-cache libcap \
    && setcap -r /usr/bin/caddy \
    && test -z "$(getcap /usr/bin/caddy)" \
    && addgroup -g 10001 router \
    && adduser -D -H -u 10001 -G router router \
    && mkdir -p /data /config \
    && chown 10001:10001 /data /config
COPY Caddyfile /etc/caddy/Caddyfile
USER 10001:10001
EXPOSE 8443
