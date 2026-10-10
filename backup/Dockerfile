# The backup service (docs/operations.md): the database's own image, so
# pg_dump and pg_restore are the server's version and the restore has the
# TimescaleDB version the dump needs, plus rclone for the optional copy
# off the machine.
ARG DB_IMAGE
FROM ${DB_IMAGE}

RUN apk add --no-cache rclone

COPY scrobblr-backup.sh /usr/local/bin/scrobblr-backup
RUN chmod 0755 /usr/local/bin/scrobblr-backup

ENTRYPOINT ["scrobblr-backup"]
CMD ["schedule"]
