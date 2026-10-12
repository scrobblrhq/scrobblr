# The backup service (docs/operations.md): the database's own image, so
# pg_dump and pg_restore are the server's version and the restore has the
# TimescaleDB version the dump needs, plus rclone for the optional copy
# off the machine. Compose passes its x-db-image; keep this default equal.
ARG DB_IMAGE=timescale/timescaledb:2.30.2-pg16
FROM ${DB_IMAGE}

RUN apk add --no-cache rclone

COPY scrobblr-backup.sh /usr/local/bin/scrobblr-backup
RUN chmod 0755 /usr/local/bin/scrobblr-backup

ENTRYPOINT ["scrobblr-backup"]
CMD ["schedule"]
